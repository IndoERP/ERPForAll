//! What the uninstall purge does to records of the `base` addon's own models (review
//! round 1, A-D2, A-D3): since `ir.ui.view` and `res.groups` always
//! stay, the records a removed module owns in them are deleted, with known outcomes for what
//! refers to them. The generic purge is in `purge_specs.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use erp_data::load::{LoadErrorKind, purge_module_data};
use erp_data::model_data::UNLINK_HOOK;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{OnDelete, Transaction, Value};
use erp_testkit::Harness;
use users_support::{installing, records, s, set_cmd, user, user_types};

const VIEW: &str = "ir.ui.view";

/// The module the tests uninstall: it adds the `gantt` view type.
static T_GONE: AddonDescriptor = AddonDescriptor {
    name: "t_gone",
    depends: &["base"],
    exact_only: false,
};
/// A module that stays: `t.keep` refers to groups with `ondelete="restrict"`.
static T_KEEP: AddonDescriptor = AddonDescriptor {
    name: "t_keep",
    depends: &["base"],
    exact_only: false,
};

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &T_GONE,
        &T_KEEP,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::extension("t_gone", VIEW, &[VIEW]).fields(
            const {
                &[FieldDecl::selection_add(
                    "type",
                    &[("gantt", Some("Gantt"))],
                )]
            },
        ),
        ModelDecl::new("t_keep", "t.keep")
            .description("Keep")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::many2one("group_id", "res.groups").ondelete(OnDelete::Restrict),
                    ]
                },
            ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

/// Bind `module.name` to record `id` of `model`.
async fn bind(tx: &mut Transaction, model: &str, xmlid: &str, id: i64) {
    let (module, name) = xmlid.split_once('.').unwrap();
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, $3, $4)",
        &[&module, &name, &model, &id],
    )
    .await
    .unwrap();
}

async fn view(tx: &mut Transaction, pairs: &[(&str, Value)]) -> i64 {
    let values: erp_sdk::ValueMap = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let views = tx.env().records(VIEW).unwrap();
    let vals = views.vals_from_values(values.iter()).unwrap();
    views.create(tx, vec![vals]).await.unwrap().ids()[0]
}

async fn alive(tx: &mut Transaction, model: &str, ids: &[i64]) -> Vec<i64> {
    records(tx, model, ids)
        .with_context([("active_test", Value::Bool(false))])
        .exists(tx)
        .await
        .unwrap()
        .ids()
}

async fn has_xid(tx: &mut Transaction, module: &str, name: &str) -> bool {
    let rows = tx
        .query(
            "SELECT 1 FROM ir_model_data WHERE module = $1 AND name = $2",
            &[&module, &name],
        )
        .await
        .unwrap();
    !rows.is_empty()
}

/// A removed module's group is deleted with its memberships; its users stay (review A-D3).
#[tokio::test]
async fn a_removed_modules_group_goes_its_users_stay() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx, true);
    let (internal, _, _) = user_types(&mut tx).await;
    let member = user(&mut tx, "member", &[internal]).await;
    let group = users_support::create(
        &mut tx,
        "res.groups",
        &[("name", s("Gone")), ("user_ids", set_cmd(&[member]))],
    )
    .await;
    bind(&mut tx, "res.groups", "t_gone.group_gone", group).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(report.deleted, [("res.groups".to_owned(), 1)]);
    assert!(alive(&mut tx, "res.groups", &[group]).await.is_empty());
    assert_eq!(alive(&mut tx, "res.users", &[member]).await, [member]);
    let rows = tx
        .query(
            "SELECT count(*) FROM res_groups_users_rel WHERE gid = $1",
            &[&group],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    assert!(!has_xid(&mut tx, "t_gone", "group_gone").await);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A record of a staying module that still names a removed module's group through a `restrict`
/// foreign key fails the uninstall with the ORM's error (review A-D3).
#[tokio::test]
async fn a_restrict_reference_of_a_staying_model_fails_the_uninstall() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx, true);
    let group = users_support::create(&mut tx, "res.groups", &[("name", s("Gone"))]).await;
    bind(&mut tx, "res.groups", "t_gone.group_gone", group).await;
    users_support::create(
        &mut tx,
        "t.keep",
        &[("name", s("keeper")), ("group_id", Value::Int(group))],
    )
    .await;
    tx.commit().await.unwrap();

    // what the ORM itself says about deleting that group
    let mut probe = h.begin().await.unwrap();
    let expected = records(&probe, "res.groups", &[group])
        .with_context([("force_delete", Value::Bool(true))])
        .unlink(&mut probe)
        .await
        .unwrap_err();
    drop(probe);

    let mut tx = h.begin().await.unwrap();
    let err = purge_module_data(&mut tx, &["t_gone"]).await.unwrap_err();
    let LoadErrorKind::Orm(orm) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(orm.kind(), expected.kind());
    assert_eq!(orm.message(), expected.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A removed module's view goes with every view built on it, the extensions of staying modules
/// included, and their external ids (the view model's `force_delete` cascade,
/// ; review A-D3: the menu-with-children case, with the view tree this
/// branch has).
#[tokio::test]
async fn a_removed_modules_view_goes_with_the_views_built_on_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("arch", Value::str("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    bind(&mut tx, VIEW, "t_gone.view_parent", parent).await;
    let child = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(parent)),
            (
                "arch",
                Value::str("<field name=\"name\" position=\"after\"/>"),
            ),
        ],
    )
    .await;
    bind(&mut tx, VIEW, "t_keep.view_child", child).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(report.deleted, [(VIEW.to_owned(), 1)]);
    assert!(alive(&mut tx, VIEW, &[parent, child]).await.is_empty());
    assert!(!has_xid(&mut tx, "t_keep", "view_child").await);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The selection values of rows the purge deletes: the removed module's view
/// of its own type, a view built on it and a copy of it, are all deleted by the purge;
/// a view of that type the purge keeps has its ondelete policies processed.
#[tokio::test]
async fn selection_values_of_rows_the_purge_deletes_are_cleaned_up() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let gantt = |extra: &[(&'static str, Value)]| {
        let mut pairs = vec![
            ("model", Value::str("res.groups")),
            ("arch", Value::str("<gantt/>")),
        ];
        pairs.extend(extra.iter().cloned());
        pairs
    };
    let own = view(&mut tx, &gantt(&[])).await;
    bind(&mut tx, VIEW, "t_gone.gantt_view", own).await;
    let child = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(own)),
            ("arch", Value::str("<gantt position=\"inside\"/>")),
        ],
    )
    .await;
    let copy = view(&mut tx, &gantt(&[("key", Value::str("t_gone.gantt_copy"))])).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(report.deleted, [(VIEW.to_owned(), 1)]);
    assert_eq!(report.copied_views_removed, 1);
    assert!(alive(&mut tx, VIEW, &[own, child, copy]).await.is_empty());

    let kept = view(&mut tx, &gantt(&[])).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(alive(&mut tx, VIEW, &[kept]).await, [kept]);
    let recs = tx.env().records(VIEW).unwrap().browse(vec![kept]).unwrap();
    let vals = recs.read(&mut tx, &["type"]).await.unwrap();
    assert_eq!(vals[0]["type"], Value::False);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The copied-view counts with the real view model (reviews A2-D2, A2-D3): a copy goes with the
/// views built on it, one of them named by a staying module (the cascade takes it: it is counted
/// removed, not kept); a copy named only by an export goes too; a copy a staying module names, not
/// built on a copy, is kept. Four removed (copy, its two children, the exported one), one kept.
/// The purge's statement count is pinned (review D-2).
#[tokio::test]
async fn copied_view_counts_follow_the_cascade() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let form = |key: &str| {
        vec![
            ("model", Value::str("res.groups")),
            ("arch", Value::str("<form><field name=\"name\"/></form>")),
            ("key", Value::str(key)),
        ]
    };
    let copy = view(&mut tx, &form("t_gone.copy")).await;
    let child = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(copy)),
            (
                "arch",
                Value::str("<field name=\"name\" position=\"after\"/>"),
            ),
        ],
    )
    .await;
    let named_child = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(copy)),
            ("key", Value::str("t_gone.named_child")),
            (
                "arch",
                Value::str("<field name=\"name\" position=\"before\"/>"),
            ),
        ],
    )
    .await;
    bind(&mut tx, VIEW, "t_keep.view_named_child", named_child).await;
    let exported = view(&mut tx, &form("t_gone.exported")).await;
    bind(&mut tx, VIEW, "__export__.ir_ui_view_9_abc", exported).await;
    let kept = view(&mut tx, &form("t_gone.kept")).await;
    bind(&mut tx, VIEW, "t_keep.view_kept", kept).await;
    let before = tx.query_count();
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(tx.query_count() - before, 10, "purge with copies");
    assert_eq!(report.copied_views_removed, 4);
    assert_eq!(report.copied_views_kept, 1);
    // reported, and its external id went with it (review A3-S3)
    let named: Vec<(i64, &str)> = report
        .copied_views_named_removed
        .iter()
        .map(|v| (v.view_id, v.xmlid.as_str()))
        .collect();
    assert_eq!(named, [(named_child, "t_keep.view_named_child")]);
    assert!(!has_xid(&mut tx, "t_keep", "view_named_child").await);
    assert!(has_xid(&mut tx, "t_keep", "view_kept").await);
    assert_eq!(
        alive(&mut tx, VIEW, &[copy, child, named_child, exported, kept]).await,
        [kept]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Every named view the cascade of a copy deletes is listed, not only those keyed `<module>.…`
/// (review R-5): a view of a staying module with no key, two levels under the copy, and one keyed
/// by another module are both reported with their external ids, and counted removed. Before the
/// fix only keyed views were examined, so these two were counted and never listed.
#[tokio::test]
async fn named_views_without_the_modules_key_under_a_copy_are_listed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let copy = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("arch", Value::str("<form><field name=\"name\"/></form>")),
            ("key", Value::str("t_gone.copy")),
        ],
    )
    .await;
    let child = |parent: i64, position: &str| {
        vec![
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(parent)),
            (
                "arch",
                Value::str(format!("<field name=\"name\" position=\"{position}\"/>")),
            ),
        ]
    };
    let plain = view(&mut tx, &child(copy, "after")).await;
    // no key (a form view), two levels under the copy
    let keyless = view(&mut tx, &child(plain, "before")).await;
    bind(&mut tx, VIEW, "t_keep.view_keyless", keyless).await;
    let mut other = child(copy, "before");
    other.push(("key", Value::str("t_keep.other")));
    let other = view(&mut tx, &other).await;
    bind(&mut tx, VIEW, "t_keep.view_other", other).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(report.copied_views_removed, 4);
    assert_eq!(report.copied_views_kept, 0);
    let named: Vec<(i64, &str)> = report
        .copied_views_named_removed
        .iter()
        .map(|v| (v.view_id, v.xmlid.as_str()))
        .collect();
    let mut expected = vec![
        (keyless, "t_keep.view_keyless"),
        (other, "t_keep.view_other"),
    ];
    expected.sort_unstable();
    assert_eq!(named, expected);
    assert!(
        alive(&mut tx, VIEW, &[copy, plain, keyless, other])
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The views a removed module's own view takes with it (its `force_delete` cascade) are reported
/// (review S-4), as those taken with a copy are: counted in `views_cascaded`, and each one a
/// staying module names listed with its external id. A view the removed module shares with a
/// staying module sits under the removed view: it is deleted, so it is listed and no longer
/// counted as kept. A view the module owns itself is deleted by the purge anyway and is not
/// counted. Before the fix only the removed module's view was counted (`deleted`).
#[tokio::test]
async fn the_views_a_removed_modules_view_takes_with_it_are_reported() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = view(
        &mut tx,
        &[
            ("model", Value::str("res.groups")),
            ("arch", Value::str("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    bind(&mut tx, VIEW, "t_gone.view_parent", parent).await;
    let under = |parent: i64, position: &str| {
        vec![
            ("model", Value::str("res.groups")),
            ("inherit_id", Value::Int(parent)),
            (
                "arch",
                Value::str(format!("<field name=\"name\" position=\"{position}\"/>")),
            ),
        ]
    };
    let child = view(&mut tx, &under(parent, "after")).await;
    bind(&mut tx, VIEW, "t_keep.view_child", child).await;
    let grandchild = view(&mut tx, &under(child, "before")).await;
    let own = view(&mut tx, &under(parent, "before")).await;
    bind(&mut tx, VIEW, "t_gone.view_own", own).await;
    let shared = view(&mut tx, &under(parent, "after")).await;
    bind(&mut tx, VIEW, "t_gone.view_shared", shared).await;
    bind(&mut tx, VIEW, "t_keep.view_shared", shared).await;
    let before = tx.query_count();
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    let statements = tx.query_count() - before;
    eprintln!("OWNED_VIEW_PURGE_STATEMENTS={statements}");
    assert_eq!(statements, 14);
    assert_eq!(report.deleted, [(VIEW.to_owned(), 2)]);
    assert_eq!(report.kept_shared, 0);
    assert_eq!(report.views_cascaded, 3);
    let named: Vec<(i64, &str)> = report
        .views_named_cascaded
        .iter()
        .map(|v| (v.view_id, v.xmlid.as_str()))
        .collect();
    assert_eq!(
        named,
        [(child, "t_keep.view_child"), (shared, "t_keep.view_shared")]
    );
    assert!(
        alive(&mut tx, VIEW, &[parent, child, grandchild, own, shared])
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A newer parent external id cascades a child from a later, separate model run.
/// Both owned views must count as deleted, while staying descendants count as cascaded.
#[tokio::test]
async fn an_owned_child_cascaded_before_its_run_is_counted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = view(
        &mut tx,
        &[
            ("model", s("res.groups")),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await;
    let child = view(
        &mut tx,
        &[
            ("model", s("res.groups")),
            ("inherit_id", Value::Int(parent)),
            ("arch", s("<field name=\"name\" position=\"after\"/>")),
        ],
    )
    .await;
    bind(&mut tx, VIEW, "t_gone.child_first", child).await;
    let separator = users_support::create(&mut tx, "t.keep", &[("name", s("separator"))]).await;
    bind(&mut tx, "t.keep", "t_gone.separator", separator).await;
    bind(&mut tx, VIEW, "t_gone.parent_last", parent).await;
    let report = purge_module_data(&mut tx, &["t_gone"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(
        report
            .deleted
            .iter()
            .filter(|(model, _)| model == VIEW)
            .map(|(_, count)| count)
            .sum::<usize>(),
        2
    );
    assert_eq!(report.views_cascaded, 0);
    assert!(alive(&mut tx, VIEW, &[parent, child]).await.is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
