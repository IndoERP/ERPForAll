//! Statements of view selection on the wire, and the
//! `templates` cache invalidation across nodes (§10 MN-1, the A part).
//!
//! Round trips are counted between the ORM's pool and the server (`WireCounter`) after a first
//! call of the same statement, so the counts are of a warm connection.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::str::FromStr;
use std::sync::Arc;

use erp_addon_base::ir_ui_view::TEMPLATES;
use erp_addon_base::ir_ui_view::select::{
    ViewId, ViewNotice, ViewRequest, ViewType, default_view, select_view,
};
use erp_data::model_data::{self, XidBinding};
use erp_data::{ImpexExt, XmlId};
use erp_orm::{Orm, OrmCacheKey, Signal, SignalKey, StaticPolicy};
use erp_sdk::{Cell, Context, RawVals, Transaction, Value};
use erp_test_models::base_users;
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::{Config, NoTls};
use erp_testkit::{Harness, assert_round_trips, database_url_from_env};

const VIEW: &str = "ir.ui.view";

fn builder() -> erp_testkit::HarnessBuilder {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

async fn create(tx: &mut Transaction, pairs: &[(&str, Cell)]) -> i64 {
    let mut vals = RawVals::new();
    for (name, cell) in pairs {
        vals.insert((*name).to_owned().into(), cell.clone());
    }
    let views = tx.env().records(VIEW).unwrap();
    views.create(tx, vec![vals]).await.unwrap().ids()[0]
}

async fn bind(tx: &mut Transaction, xid: &str, id: i64) {
    let env = tx.env();
    let binding = XidBinding::new(XmlId::parse(xid).unwrap(), VIEW, id, false);
    model_data::update_xmlids(tx, &env, &[binding], false)
        .await
        .unwrap();
}

/// `default_view` is one statement; `select_view` with a qualified ref that names a view of the
/// request is one (the lookup joined with the view), and one more for the default when the ref
/// cannot be used; a given view costs one (its model and type are checked, review A-T5).
#[tokio::test]
async fn view_selection_statement_counts() {
    let h = builder().count_round_trips().build().await.unwrap();
    let wire = h.wire().expect("built with count_round_trips").clone();
    let mut tx = h.begin().await.unwrap();
    let form = || {
        vec![
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ]
    };
    let default = create(&mut tx, &form()).await;
    let named = create(&mut tx, &form()).await;
    bind(&mut tx, "base.named_form", named).await;
    let list = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<list><field name=\"name\"/></list>")),
        ],
    )
    .await;
    bind(&mut tx, "base.named_list", list).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let form = ViewType::new(&tx, "form").unwrap();
    let qualified = Context::from_pairs([("form_view_ref", Value::str("base.named_form"))]);
    let other_type = Context::from_pairs([("form_view_ref", Value::str("base.named_list"))]);
    // warm: each statement once
    default_view(&mut tx, Some(VIEW), &form).await.unwrap();
    for context in [&qualified, &other_type] {
        select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(context))
            .await
            .unwrap()
            .id();
    }

    let before = tx.query_count();
    let found = assert_round_trips!(wire, == 1, {
        default_view(&mut tx, Some(VIEW), &form).await.unwrap()
    });
    assert_eq!(tx.query_count() - before, 1);
    assert_eq!(found, Some(ViewId::new(default).unwrap()));

    let before = tx.query_count();
    let picked = assert_round_trips!(wire, == 1, {
        select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(&qualified))
            .await
            .unwrap()
    });
    assert_eq!(tx.query_count() - before, 1);
    assert_eq!(picked.id(), ViewId::new(named).unwrap());

    let before = tx.query_count();
    let picked = assert_round_trips!(wire, == 2, {
        select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(&other_type))
            .await
            .unwrap()
    });
    assert_eq!(tx.query_count() - before, 2);
    assert_eq!(picked.id(), ViewId::new(default).unwrap());
    assert!(matches!(
        picked.notices(),
        [ViewNotice::RefOtherType { view_type, .. }] if view_type.as_deref() == Some("list")
    ));

    let given = ViewRequest::new(VIEW, &form).view_id(ViewId::new(named).unwrap());
    select_view(&mut tx, &given).await.unwrap().id();
    let before = tx.query_count();
    let picked = assert_round_trips!(wire, == 1, {
        select_view(&mut tx, &given).await.unwrap()
    });
    assert_eq!(tx.query_count() - before, 1);
    assert_eq!(picked.id(), ViewId::new(named).unwrap());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The statement `default_view` sends is served by the `_default_view` partial index (review
/// A-D1): the query repeats the index predicate (`active IS TRUE`). The table of a test database
/// is tiny, so the planner would scan it whatever the indexes: the check runs with
/// `enable_seqscan = off` for its transaction, which leaves the planner the indexes whose
/// predicate the query implies, and is deterministic.
#[tokio::test]
async fn default_view_uses_its_partial_index() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let rows = tx
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'ir_ui_view' \
               AND indexname LIKE '%default_view%' AND indexdef LIKE '%WHERE%'",
            &[],
        )
        .await
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!(
            "one partial _default_view index expected, found {}",
            rows.len()
        )
    };
    let index: String = row.get(0);
    let form = ViewType::new(&tx, "form").unwrap();
    tx.capture_statements(true);
    default_view(&mut tx, Some(VIEW), &form).await.unwrap();
    let sql = tx
        .captured()
        .iter()
        .rev()
        .find(|q| q.contains("FROM ir_ui_view"))
        .expect("the default view statement")
        .replace("$1", &format!("'{VIEW}'"))
        .replace("$2", "'form'");
    tx.capture_statements(false);
    tx.batch("SET LOCAL enable_seqscan = off").await.unwrap();
    let plan: Vec<String> = tx
        .query(&format!("EXPLAIN {sql}"), &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    let plan = plan.join("\n");
    assert!(plan.contains(&index), "{index} not used:\n{plan}");
    assert!(!plan.contains("Seq Scan"), "{plan}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Two transactions that make two root views each other's parent (A on B, B on A) cannot both
/// commit: each check locks the roots it reaches, so the second one fails (a serialization or
/// deadlock error) and no cycle is stored (review A-D4).
#[tokio::test]
async fn concurrent_writes_cannot_commit_a_cycle() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let form = || {
        vec![
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ]
    };
    let a = create(&mut tx, &form()).await;
    let b = create(&mut tx, &form()).await;
    tx.commit().await.unwrap();

    let write = async |tx: &mut Transaction, child: i64, parent: i64| {
        let views = tx.env().records(VIEW)?.browse([child])?;
        views
            .write(
                tx,
                RawVals::new()
                    .set_cell("inherit_id", Cell::Id(parent))
                    .set_cell("arch", s("<form position=\"inside\"/>")),
            )
            .await?;
        // the write is flushed and checked: the check's locks are held from here
        tx.flush().await
    };
    let mut one = h.begin().await.unwrap();
    let mut two = h.begin().await.unwrap();
    // both snapshots are taken before either writes
    one.query("SELECT 1", &[]).await.unwrap();
    two.query("SELECT 1", &[]).await.unwrap();
    write(&mut one, a, b).await.unwrap();
    let (second, first) = tokio::join!(write(&mut two, b, a), async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        one.commit().await
    });
    first.unwrap();
    let err = second.expect_err("the second cycle half must fail");
    assert!(matches!(err.sqlstate(), Some("40001" | "40P01")), "{err:?}");
    drop(two);
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT id, inherit_id FROM ir_ui_view WHERE id = ANY($1) ORDER BY id",
            &[&vec![a, b]],
        )
        .await
        .unwrap();
    let parents: Vec<(i64, Option<i64>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(parents, [(a, Some(b)), (b, None)]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The error a transaction gets when a root it locks was written by a transaction that committed
/// after its snapshot: `Concurrency`, SQLSTATE `40001`. The message is the server's, in the
/// cluster's `lc_messages` language, so it is not compared (review A3-R7): the kind and the
/// SQLSTATE identify the error.
fn assert_serialization_failure(err: &erp_sdk::Error) {
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Concurrency, "{err:?}");
    assert_eq!(err.sqlstate(), Some("40001"), "{err:?}");
}

/// Write `vals` on view `id` and flush, so the write's check (and its locks) has run.
async fn write_view(tx: &mut Transaction, id: i64, vals: RawVals) -> erp_sdk::Result<()> {
    let views = tx.env().records(VIEW)?.browse([id])?;
    views.write(tx, vals).await?;
    tx.flush().await
}

/// Run `second` on `two` while `one` commits 300 ms later; both snapshots are taken first. The
/// commit of `one` must succeed; the result of `second` is returned.
async fn race<F>(mut one: Transaction, two: &mut Transaction, second: F) -> erp_sdk::Result<()>
where
    F: AsyncFnOnce(&mut Transaction) -> erp_sdk::Result<()>,
{
    let (result, committed) = tokio::join!(second(two), async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        one.commit().await
    });
    committed.unwrap();
    result
}

/// `(id, inherit_id, arch_db)` of `ids` as committed.
async fn committed_views(h: &Harness, ids: &[i64]) -> Vec<(i64, Option<i64>, String)> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT id, inherit_id, arch_db FROM ir_ui_view WHERE id = ANY($1) ORDER BY id",
            &[&ids.to_vec()],
        )
        .await
        .unwrap();
    let views = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    tx.rollback().await.unwrap();
    views
}

/// Review A2-D1: a cycle made through two extensions of two different trees (X under root A, Y
/// under root B; one transaction puts X under Y, the other Y under X). Each write locks the roots
/// of the trees it touches and writes them, so the transaction whose snapshot predates the
/// other's commit fails with a serialization error and no cycle is stored.
#[tokio::test]
async fn concurrent_writes_cannot_commit_a_cycle_through_extensions() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = || {
        vec![
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ]
    };
    let a = create(&mut tx, &root()).await;
    let b = create(&mut tx, &root()).await;
    let extension = |parent: i64| {
        vec![
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(parent)),
            ("arch", s("<form position=\"inside\"/>")),
        ]
    };
    let x = create(&mut tx, &extension(a)).await;
    let y = create(&mut tx, &extension(b)).await;
    tx.commit().await.unwrap();

    let reparent = |parent: i64| RawVals::new().set_cell("inherit_id", Cell::Id(parent));
    let mut one = h.begin().await.unwrap();
    let mut two = h.begin().await.unwrap();
    one.query("SELECT 1", &[]).await.unwrap();
    two.query("SELECT 1", &[]).await.unwrap();
    write_view(&mut one, x, reparent(y)).await.unwrap();
    let err = race(one, &mut two, async |two| {
        write_view(two, y, reparent(x)).await
    })
    .await
    .expect_err("the second half of the cycle must fail");
    assert_serialization_failure(&err);
    drop(two);
    let parents: Vec<(i64, Option<i64>)> = committed_views(&h, &[x, y])
        .await
        .into_iter()
        .map(|(id, parent, _)| (id, parent))
        .collect();
    assert_eq!(parents, [(x, Some(y)), (y, Some(b))]);
    h.cleanup().await.unwrap();
}

/// Review A2-D1, second case: a child created under an extension while another transaction
/// rewrites that extension so the child's target is gone. The rewrite writes the tree's root, so
/// the create's check fails with a serialization error instead of committing a child that no
/// longer applies.
#[tokio::test]
async fn a_child_cannot_be_created_while_its_parent_changes() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let parent = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            (
                "arch",
                s("<field name=\"name\" position=\"after\"><field name=\"model\"/></field>"),
            ),
        ],
    )
    .await;
    tx.commit().await.unwrap();

    let mut one = h.begin().await.unwrap();
    let mut two = h.begin().await.unwrap();
    one.query("SELECT 1", &[]).await.unwrap();
    two.query("SELECT 1", &[]).await.unwrap();
    // the parent stops adding `model`
    write_view(
        &mut one,
        parent,
        RawVals::new().set_cell(
            "arch",
            s("<field name=\"name\" position=\"after\"><field name=\"priority\"/></field>"),
        ),
    )
    .await
    .unwrap();
    let err = race(one, &mut two, async |two| {
        let mut vals = RawVals::new();
        vals.insert("model".to_owned().into(), s(VIEW));
        vals.insert("inherit_id".to_owned().into(), Cell::Id(parent));
        vals.insert(
            "arch".to_owned().into(),
            s("<field name=\"model\" position=\"after\"><field name=\"key\"/></field>"),
        );
        two.env().records(VIEW)?.create(two, vec![vals]).await?;
        two.flush().await
    })
    .await
    .expect_err("the child of a changed parent must fail");
    assert_serialization_failure(&err);
    drop(two);
    let mut tx = h.begin().await.unwrap();
    let children = tx
        .query(
            "SELECT count(*) FROM ir_ui_view WHERE inherit_id = $1",
            &[&parent],
        )
        .await
        .unwrap();
    assert_eq!(children[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Review A2-R1: two sibling extensions of one root rewritten at once, the second so it relies on
/// what the first stops adding. Both checks write the root, so the second fails with a
/// serialization error and the committed tree still combines.
#[tokio::test]
async fn sibling_extensions_cannot_be_rewritten_concurrently() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let adds_model = "<field name=\"name\" position=\"after\"><field name=\"model\"/></field>";
    let first = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s(adds_model)),
        ],
    )
    .await;
    let second = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await;
    tx.commit().await.unwrap();

    let mut one = h.begin().await.unwrap();
    let mut two = h.begin().await.unwrap();
    one.query("SELECT 1", &[]).await.unwrap();
    two.query("SELECT 1", &[]).await.unwrap();
    write_view(
        &mut one,
        first,
        RawVals::new().set_cell("arch", s("<form position=\"inside\"/>")),
    )
    .await
    .unwrap();
    let relies = "<field name=\"model\" position=\"after\"><field name=\"key\"/></field>";
    let err = race(one, &mut two, async |two| {
        write_view(two, second, RawVals::new().set_cell("arch", s(relies))).await
    })
    .await
    .expect_err("the second sibling rewrite must fail");
    assert_serialization_failure(&err);
    drop(two);
    let archs: Vec<(i64, String)> = committed_views(&h, &[first, second])
        .await
        .into_iter()
        .map(|(id, _, arch)| (id, arch))
        .collect();
    assert_eq!(
        archs,
        [
            (first, "<form position=\"inside\"/>".to_owned()),
            (second, "<form position=\"inside\"/>".to_owned()),
        ]
    );
    h.cleanup().await.unwrap();
}

/// `check_views` locks and writes the roots of the trees it checks, so a read-only transaction is
/// refused up front with an error that says why (reviews A3-S4, A3-R1, A3-D3a), not with the raw
/// statement guard's error from the middle of the check.
#[tokio::test]
async fn the_view_check_needs_a_write_transaction() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let mut tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID).read_only("ir.ui.view.read"))
        .await
        .unwrap();
    let records = tx.env().records(VIEW).unwrap().browse([id]).unwrap();
    let err = erp_addon_base::ir_ui_view::check_views(&mut tx, &records, &[id])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming, "{err:?}");
    assert_eq!(
        err.message(),
        "ir.ui.view: the view check locks and writes the roots of the trees it checks, so \
         that concurrent writers of a tree fail; it needs a write transaction, not a read-only \
         one"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Row versions a transaction writes into `ir_ui_view`: `(updated, hot updated)` from the
/// server's per-transaction counters.
async fn view_updates(tx: &mut Transaction) -> (i64, i64) {
    let rows = tx
        .query(
            "SELECT pg_stat_get_xact_tuples_updated('ir_ui_view'::regclass), \
                    pg_stat_get_xact_tuples_hot_updated('ir_ui_view'::regclass)",
            &[],
        )
        .await
        .unwrap();
    (rows[0].get(0), rows[0].get(1))
}

/// Ten extensions of one root rewritten one at a time in one transaction: each write is checked,
/// and the root is written once, not once per check (review A3-D1: a root this transaction
/// already wrote is a row a concurrent writer conflicts on, so it is skipped). Pinned at the
/// measured counts: ten extension updates and one root update, all HOT (no indexed column
/// changes). Before the fix: twenty updates.
#[tokio::test]
async fn a_root_is_written_once_per_transaction() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let mut extensions = Vec::new();
    for _ in 0..10 {
        extensions.push(
            create(
                &mut tx,
                &[
                    ("model", s(VIEW)),
                    ("inherit_id", Cell::Id(root)),
                    ("arch", s("<form position=\"inside\"/>")),
                ],
            )
            .await,
        );
    }
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let before = view_updates(&mut tx).await;
    for id in &extensions {
        write_view(
            &mut tx,
            *id,
            RawVals::new().set_cell("arch", s("<form position=\"inside\"><div/></form>")),
        )
        .await
        .unwrap();
    }
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (11, 11));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Ten extensions rewritten inside individual savepoints: each savepoint check tracks touched
/// roots in the transaction, so the root is written once for the whole transaction rather than
/// once per savepoint (review D-7). Pinned at ten extension updates and one
/// root update, all HOT (11, 11). Not run red against the old code.
#[tokio::test]
async fn a_root_is_written_once_per_transaction_across_savepoints() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let mut extensions = Vec::new();
    for _ in 0..10 {
        extensions.push(
            create(
                &mut tx,
                &[
                    ("model", s(VIEW)),
                    ("inherit_id", Cell::Id(root)),
                    ("arch", s("<form position=\"inside\"/>")),
                ],
            )
            .await,
        );
    }
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let before = view_updates(&mut tx).await;
    for id in &extensions {
        let id = *id;
        tx.savepoint(async move |tx: &mut Transaction| {
            write_view(
                tx,
                id,
                RawVals::new().set_cell("arch", s("<form position=\"inside\"><div/></form>")),
            )
            .await
        })
        .await
        .unwrap();
    }
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (11, 11));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Importing updates to ten view extensions through `impex::load` writes the root once across the
/// import's savepoints (review D-7). `impex/load.rs` writes existing records one
/// by one inside a batch savepoint; with per-transaction root tracking the root is written once on
/// the first record and skipped on the remaining nine. Pinned at ten extension updates and one
/// root update, all HOT (11, 11). Not run red against the old code.
#[tokio::test]
async fn a_root_is_written_once_per_transaction_on_import() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("name", s("root_view")),
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let mut extensions = Vec::new();
    for i in 0..10 {
        extensions.push(
            create(
                &mut tx,
                &[
                    ("name", s(&format!("ext_{i}"))),
                    ("model", s(VIEW)),
                    ("inherit_id", Cell::Id(root)),
                    ("arch", s("<form position=\"inside\"/>")),
                ],
            )
            .await,
        );
    }
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let before = view_updates(&mut tx).await;
    let views = tx
        .env()
        .records(VIEW)
        .unwrap()
        .with_context([("import_file", Value::Bool(true))]);
    let fields = &[".id", "arch"];
    let data: Vec<Vec<String>> = extensions
        .iter()
        .map(|id| {
            vec![
                id.to_string(),
                "<form position=\"inside\"><span/></form>".to_owned(),
            ]
        })
        .collect();
    let result = views.load(&mut tx, fields, &data).await.unwrap();
    assert_eq!(result.messages, vec![]);
    assert_eq!(result.ids, Some(extensions));
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (11, 11));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A root touched inside a savepoint that rolls back is rewritten by a subsequent check outside
/// the savepoint (review D-7): rollback resets roots touched at deeper levels.
#[tokio::test]
async fn a_root_touched_in_a_rolled_back_savepoint_is_rewritten() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let ext = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();

    // Touch the root inside a savepoint that fails and rolls back
    let failed: erp_sdk::Result<()> = tx
        .savepoint(async move |tx: &mut Transaction| {
            write_view(
                tx,
                ext,
                RawVals::new().set_cell("arch", s("<form position=\"inside\"><div/></form>")),
            )
            .await?;
            Err(erp_sdk::Error::user("rollback savepoint"))
        })
        .await;
    let err = failed.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::UserError, "{err:?}");
    assert!(err.message().contains("rollback savepoint"), "{err:?}");

    // Outside the savepoint, root was reset; write_view writes both ext and root (2 updates)
    let before = view_updates(&mut tx).await;
    write_view(
        &mut tx,
        ext,
        RawVals::new().set_cell("arch", s("<form position=\"inside\"><p/></form>")),
    )
    .await
    .unwrap();
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (2, 2));

    // A second write in the same transaction skips the already written root (1 update)
    let before = view_updates(&mut tx).await;
    write_view(
        &mut tx,
        ext,
        RawVals::new().set_cell("arch", s("<form position=\"inside\"><b/></form>")),
    )
    .await
    .unwrap();
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A root touched inside an isolated (onchange) scope is forgotten when the scope ends: the scope
/// is rolled back, so a check outside it writes the root again. Not run red
/// against the old code.
#[tokio::test]
async fn a_root_touched_in_an_isolated_scope_is_rewritten_after_it() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let ext = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();

    // The write inside the scope writes the extension and the root; the scope then rolls back.
    tx.isolated_for_tests(async move |tx: &mut Transaction| {
        write_view(
            tx,
            ext,
            RawVals::new().set_cell("arch", s("<form position=\"inside\"><div/></form>")),
        )
        .await
    })
    .await
    .unwrap();

    // Outside the scope the inner touch is forgotten: extension and root are written (2 updates).
    let before = view_updates(&mut tx).await;
    write_view(
        &mut tx,
        ext,
        RawVals::new().set_cell("arch", s("<form position=\"inside\"><p/></form>")),
    )
    .await
    .unwrap();
    let after = view_updates(&mut tx).await;
    assert_eq!((after.0 - before.0, after.1 - before.1), (2, 2));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An addon that turns the row version on for `ir.ui.view`.
static T_ROW_VERSION: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "t_view_rv",
    depends: &["base"],
    exact_only: false,
};

/// The refusal of a row-versioned `ir.ui.view` (review A3-R2).
const ROW_VERSION_REFUSED: &str = "ir.ui.view cannot have a row version (row_version = true): its view check writes the roots of the trees it checks with their own values, \
     which would bump their versions on every check";

/// `ir.ui.view` with a row version is refused (review A3-R2): by `_register_hook`, which the
/// module system calls on every final registry, and by the view check itself, so no root's
/// version is ever bumped by a check.
#[tokio::test]
async fn a_row_versioned_view_model_is_refused() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &T_ROW_VERSION,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        erp_registry::ModelDecl::extension("t_view_rv", VIEW, &[VIEW]).row_version(true),
    ])
    .unlink_hook(model_data::UNLINK_HOOK)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = erp_orm::model_hooks::call_register_hook(&mut tx, VIEW)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming, "{err:?}");
    assert_eq!(err.message(), ROW_VERSION_REFUSED);
    let views = tx.env().records(VIEW).unwrap();
    let mut vals = RawVals::new();
    vals.insert("model".to_owned().into(), s(VIEW));
    vals.insert(
        "arch".to_owned().into(),
        s("<form><field name=\"name\"/></form>"),
    );
    let err = tx
        .savepoint(async |tx: &mut Transaction| views.create(tx, vec![vals]).await.map(|_| ()))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming, "{err:?}");
    assert_eq!(err.message(), ROW_VERSION_REFUSED);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The plans of the two lock statements, as a server plans them once it switches to a generic
/// plan (after five runs of a prepared statement): the walks use the primary key and the
/// `inherit_id` index, never a sequential scan of `ir_ui_view` (review A3-D6). The test table is
/// tiny, so the check runs with `enable_seqscan = off`, which leaves the planner the indexes the
/// statement can use and is deterministic.
#[tokio::test]
async fn the_lock_statements_use_the_indexes() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let other = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let ext = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    // a write of `inherit_id` sends the lock alone, then the check sends the lock and fetch
    write_view(
        &mut tx,
        ext,
        RawVals::new().set_cell("inherit_id", Cell::Id(other)),
    )
    .await
    .unwrap();
    let locks: Vec<String> = tx
        .captured()
        .iter()
        .filter(|q| q.contains("FOR NO KEY UPDATE"))
        .cloned()
        .collect();
    tx.capture_statements(false);
    let lock_alone = locks
        .iter()
        .find(|q| !q.contains("down("))
        .expect("the lock statement");
    let lock_and_fetch = locks
        .iter()
        .find(|q| q.contains("down("))
        .expect("the lock and fetch statement");
    let rows = tx
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'ir_ui_view' \
               AND indexdef LIKE '%(inherit_id)'",
            &[],
        )
        .await
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!("one inherit_id index expected, found {}", rows.len())
    };
    let inherit_index: String = row.get(0);
    tx.batch("SET LOCAL enable_seqscan = off").await.unwrap();
    tx.batch("SET LOCAL plan_cache_mode = force_generic_plan")
        .await
        .unwrap();
    for (name, sql, args) in [
        (
            "lock_alone",
            lock_alone,
            format!("ARRAY[{ext}]::int8[], 128, ARRAY[]::int8[]"),
        ),
        (
            "lock_and_fetch",
            lock_and_fetch,
            format!(
                "ARRAY[{ext}]::int8[], 128, 10001, {}, ARRAY[]::int8[]",
                erp_sdk::views::ViewLimits::default()
                    .get(erp_sdk::views::LimitKind::CatalogueBytes)
            ),
        ),
    ] {
        tx.batch(&format!("PREPARE {name} AS {sql}")).await.unwrap();
        let plan: Vec<String> = tx
            .query(&format!("EXPLAIN EXECUTE {name}({args})"), &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        let plan = plan.join("\n");
        assert!(!plan.contains("Seq Scan"), "{name}:\n{plan}");
        assert!(plan.contains("ir_ui_view_pkey"), "{name}:\n{plan}");
        if name == "lock_and_fetch" {
            // the walk down scans the `inherit_id` index by name, not a filter mentioning the
            // column (reviews A4-D2, A4-R7)
            assert!(
                plan.lines()
                    .any(|l| l.contains("Index") && l.contains(inherit_index.as_str())),
                "{name}: no scan of {inherit_index}:\n{plan}"
            );
        }
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The losing side of a race run through `Orm::run` (review A3-D6): its first attempt fails
/// with the serialization error, the retry sees the winner's commit, and its check refuses the
/// cycle the two writes would make. The committed state is the winner's.
#[tokio::test]
async fn the_losing_side_retried_by_orm_run_sees_the_winner() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let form = || {
        vec![
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ]
    };
    let a = create(&mut tx, &form()).await;
    let b = create(&mut tx, &form()).await;
    tx.commit().await.unwrap();
    let reparent = |parent: i64| {
        RawVals::new()
            .set_cell("inherit_id", Cell::Id(parent))
            .set_cell("arch", s("<form position=\"inside\"/>"))
    };
    let mut one = h.begin().await.unwrap();
    one.query("SELECT 1", &[]).await.unwrap();
    write_view(&mut one, a, reparent(b)).await.unwrap();
    // the first attempt commits `one` once its own write waits for `one`'s lock (no sleep:
    // review A4-D3), then reports the serialization error it got
    let one = std::sync::Mutex::new(Some(one));
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let lost = h
        .run(async |tx: &mut Transaction| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if attempt > 1 {
                return write_view(tx, b, reparent(a)).await;
            }
            let pid = erp_testkit::backend_pid(tx)
                .await
                .map_err(|e| erp_sdk::Error::programming(e.to_string()))?;
            let Some(mut winner) = one.lock().unwrap().take() else {
                return Err(erp_sdk::Error::programming("the winner was taken twice"));
            };
            let (ours, committed) = tokio::join!(write_view(tx, b, reparent(a)), async {
                erp_testkit::wait_until_blocked(&h, pid, std::time::Duration::from_secs(20))
                    .await
                    .map_err(|e| erp_sdk::Error::programming(e.to_string()))?;
                winner.commit().await
            });
            committed?;
            let err = ours.expect_err("the first attempt loses");
            assert_serialization_failure(&err);
            Err(err)
        })
        .await;
    let err = match lost.unwrap_err() {
        erp_testkit::HarnessError::Orm(e) => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation, "{err:?}");
    assert_eq!(
        err.message(),
        "You cannot create recursive inherited views."
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    let parents: Vec<(i64, Option<i64>)> = committed_views(&h, &[a, b])
        .await
        .into_iter()
        .map(|(id, parent, _)| (id, parent))
        .collect();
    assert_eq!(parents, [(a, Some(b)), (b, None)]);
    h.cleanup().await.unwrap();
}

fn pool_for(h: &Harness) -> Pool {
    let mut config = Config::from_str(&database_url_from_env().unwrap()).unwrap();
    config.dbname(h.db().name());
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap()
}

async fn generation(h: &Harness, key: &SignalKey) -> u64 {
    let client = h.db().client().await.unwrap();
    Signal::read(&client).await.unwrap().get(key)
}

/// MN-1 (A part): node 2 changes an extension's arch; the commit
/// bumps `cache:templates`, and node 1's next transaction no longer sees what it had cached in
/// `templates` (named caches send no NOTIFY: the opening flight reads the generation).
#[tokio::test]
async fn view_writes_invalidate_templates_across_nodes() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let one = node(&h).await;
    let two = node(&h).await;
    let key = OrmCacheKey::new(VIEW, "get_view", vec![Value::Int(1)]);
    let templates = SignalKey::cache(TEMPLATES).unwrap();

    // node 2 creates a form and an extension
    let mut tx = two.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let base = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let ext = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(base)),
            ("arch", s("<field name=\"name\" position=\"after\"/>")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let created = generation(&h, &templates).await;
    assert!(created >= 1, "the creates bumped cache:templates");

    // node 1 caches a compiled view (after its opening flight applied node 2's creates)
    let mut tx = one.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    tx.ormcache_add(TEMPLATES, key.clone(), Value::str("compiled"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = one.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    assert_eq!(
        tx.ormcache_get(TEMPLATES, &key).await.unwrap(),
        Some(Value::str("compiled"))
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // node 2 changes the extension's arch
    let mut tx = two.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let views = tx.env().records(VIEW).unwrap().browse([ext]).unwrap();
    let arch = "<field name=\"name\" position=\"before\"><field name=\"model\"/></field>";
    views
        .write(&mut tx, RawVals::new().set_cell("arch", s(arch)))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(generation(&h, &templates).await, created + 1);

    // node 1's next transaction sees the bump: the cached value is gone, the new arch is read
    let mut tx = one.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    assert_eq!(tx.ormcache_get(TEMPLATES, &key).await.unwrap(), None);
    let views = tx.env().records(VIEW).unwrap().browse([ext]).unwrap();
    assert_eq!(views.cells(&mut tx, "arch_db").await.unwrap(), [s(arch)]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A node: an ORM with signals on the harness database, its applied baseline what the table
/// holds now.
async fn node(h: &Harness) -> Orm {
    let client = h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool_for(h))
        .policy(StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .signal(Signal::new(baseline))
        .build()
        .unwrap()
}
