//! `ir.ui.view` records in the database (§9),
//! 043, 057, 058, 059 on the `base` addon's model, written and read back through the ORM.
//! Expected values are the spec's literals or ids our `create` returned.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::error::Error as _;
use std::sync::Arc;

use erp_addon_base::ir_ui_view::get_view;
use erp_addon_base::ir_ui_view::select::{
    ViewId, ViewNotice, ViewRequest, ViewType, default_view, select_view,
};
use erp_data::load::{FileContext, LoadSession, NoFiles, SessionOptions};
use erp_data::model_data::{self, XidBinding};
use erp_data::{ConvertMode, XmlDataReader, XmlId};
use erp_sdk::views::record::{InvalidView, ViewMode, ViewRecordError, ViewRow};
use erp_sdk::views::{ViewCatalogue, ViewError};
use erp_sdk::{
    Cell, Command, Context, ErrorKind, MAX_NOTICES_PER_TRANSACTION, Notice, NoticeKind,
    QualifiedXmlId, RawVals, RecId, Transaction, Value,
};
use erp_test_models::base_users;
use erp_testkit::{FieldPolicy, Harness};

const VIEW: &str = "ir.ui.view";

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_view_groups::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

async fn harness_with_policy(policy: FieldPolicy) -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_view_groups::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
    .policy(policy)
    .build()
    .await
    .expect("harness")
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

/// `vals` as the raw values of a view write.
fn vals(pairs: &[(&str, Cell)]) -> RawVals {
    let mut vals = RawVals::new();
    for (name, cell) in pairs {
        vals.insert((*name).to_owned().into(), cell.clone());
    }
    vals
}

async fn create(tx: &mut Transaction, pairs: &[(&str, Cell)]) -> erp_sdk::Result<i64> {
    let views = tx.env().records(VIEW)?;
    Ok(views.create(tx, vec![vals(pairs)]).await?.ids()[0])
}

async fn write(tx: &mut Transaction, ids: &[i64], pairs: &[(&str, Cell)]) -> erp_sdk::Result<()> {
    let views = tx.env().records(VIEW)?.browse(ids.iter().copied())?;
    views.write(tx, vals(pairs)).await
}

/// `create` in a savepoint, so a refused create leaves the transaction usable.
async fn try_create(tx: &mut Transaction, pairs: &[(&str, Cell)]) -> erp_sdk::Error {
    let pairs = pairs.to_vec();
    tx.savepoint(async move |tx: &mut Transaction| create(tx, &pairs).await)
        .await
        .expect_err("the create must be refused")
}

async fn try_write(tx: &mut Transaction, ids: &[i64], pairs: &[(&str, Cell)]) -> erp_sdk::Error {
    let pairs = pairs.to_vec();
    let ids = ids.to_vec();
    tx.savepoint(async move |tx: &mut Transaction| write(tx, &ids, &pairs).await)
        .await
        .expect_err("the write must be refused")
}

async fn text_field(tx: &mut Transaction, id: i64, field: &str) -> Option<String> {
    let views = tx.env().records(VIEW).unwrap().browse([id]).unwrap();
    match &views.cells(tx, field).await.unwrap()[0] {
        Cell::Str(text) => Some(text.to_string()),
        Cell::Null => None,
        other => panic!("{field} of view {id} is {other:?}"),
    }
}

async fn mode(tx: &mut Transaction, id: i64) -> String {
    text_field(tx, id, "mode").await.expect("a mode")
}

/// The combined arch of `id` from the views `ids`, as read back through the ORM.
async fn combined(tx: &mut Transaction, ids: &[i64], id: i64) -> String {
    let views = tx
        .env()
        .records(VIEW)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    let archs = views.cells(tx, "arch_db").await.unwrap();
    let parents = views.cells(tx, "inherit_id").await.unwrap();
    let modes = views.cells(tx, "mode").await.unwrap();
    let actives = views.cells(tx, "active").await.unwrap();
    let priorities = views.cells(tx, "priority").await.unwrap();
    let mut rows = Vec::new();
    for (i, view) in ids.iter().enumerate() {
        let parent = match &parents[i] {
            Cell::Id(p) => Some(*p),
            Cell::Null => None,
            other => panic!("{other:?}"),
        };
        let Cell::Str(m) = &modes[i] else { panic!() };
        let Cell::Str(arch) = &archs[i] else { panic!() };
        let Cell::Bool(active) = actives[i] else {
            panic!()
        };
        let Cell::Int(priority) = priorities[i] else {
            panic!()
        };
        rows.push(
            ViewRow::new(*view, ViewMode::parse(m).unwrap())
                .arch(arch.to_string())
                .inherit_id(parent)
                .priority(priority)
                .active(active)
                .into_descriptor()
                .unwrap(),
        );
    }
    ViewCatalogue::new(rows)
        .unwrap()
        .combine(id)
        .unwrap()
        .to_xml()
}

async fn bind(tx: &mut Transaction, model: &str, xid: &str, id: i64) {
    let env = tx.env();
    let binding = XidBinding::new(XmlId::parse(xid).unwrap(), model, id, false);
    model_data::update_xmlids(tx, &env, &[binding], false)
        .await
        .unwrap();
}

async fn xid_id(tx: &mut Transaction, xid: &str) -> i64 {
    let env = tx.env();
    let key = XmlId::parse(xid).unwrap();
    model_data::lookup(tx, &env, std::slice::from_ref(&key))
        .await
        .unwrap()[&key]
        .res_id
        .unwrap()
}

/// The typed record error under an ORM error.
fn record_error(error: &erp_sdk::Error) -> &ViewRecordError {
    error
        .source()
        .and_then(|e| e.downcast_ref::<ViewRecordError>())
        .unwrap_or_else(|| panic!("no ViewRecordError under {error:?}"))
}

/// A context value of an error, or an error naming the key when the error has no context map or
/// the map has no such key (review F-7: a missing key is never read as `False`, so an assertion
/// on a value, `False` included, fails when the key is absent). Callers compare with `Ok(..)`.
fn context_of(error: &erp_sdk::Error, key: &str) -> Result<Value, String> {
    match error.context() {
        Some(Value::Map(map)) => map.get(key).cloned().ok_or_else(|| {
            let keys: Vec<&String> = map.keys().collect();
            format!("context key {key:?} missing on {error}; keys present: {keys:?}")
        }),
        other => Err(format!("no context map on {error}: {other:?}")),
    }
}

const INHERITANCE_MODE: &str =
    "Invalid inheritance mode: if the mode is 'extension', the view must extend an other view";

#[tokio::test]
async fn view_mode_defaults_and_switching() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let qweb = || ("arch", s("<qweb/>"));

    // implicit
    let view = create(&mut tx, &[qweb()]).await.unwrap();
    assert_eq!(mode(&mut tx, view).await, "primary");
    let view2 = create(&mut tx, &[("inherit_id", Cell::Id(view)), qweb()])
        .await
        .unwrap();
    assert_eq!(mode(&mut tx, view2).await, "extension");
    write(&mut tx, &[view2], &[("inherit_id", Cell::Null)])
        .await
        .unwrap();
    assert_eq!(mode(&mut tx, view2).await, "primary");
    write(&mut tx, &[view2], &[("inherit_id", Cell::Id(view))])
        .await
        .unwrap();
    assert_eq!(mode(&mut tx, view2).await, "extension");

    // explicit
    let view3 = create(
        &mut tx,
        &[
            ("inherit_id", Cell::Id(view)),
            ("mode", s("primary")),
            qweb(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(mode(&mut tx, view3).await, "primary");
    let err = try_create(&mut tx, &[("mode", s("extension")), qweb()]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("The database refused the change: {INHERITANCE_MODE}")
    );

    // a view without parent cannot become an extension
    let err = try_write(&mut tx, &[view], &[("mode", s("extension"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("The database refused the change: {INHERITANCE_MODE}")
    );

    // an explicit primary keeps its mode when re-parented
    let base2 = create(&mut tx, &[qweb()]).await.unwrap();
    write(&mut tx, &[view3], &[("inherit_id", Cell::Id(base2))])
        .await
        .unwrap();
    assert_eq!(mode(&mut tx, view3).await, "primary");

    // per record: the view without parent becomes an extension, the re-parented primary stays
    let lone = create(&mut tx, &[qweb()]).await.unwrap();
    write(&mut tx, &[lone, view3], &[("inherit_id", Cell::Id(view))])
        .await
        .unwrap();
    assert_eq!(mode(&mut tx, lone).await, "extension");
    assert_eq!(mode(&mut tx, view3).await, "primary");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `testDefaultViewBase`, `testDefaultViewPrimary` and `test_view_ref`: the primary view with the
/// lowest priority wins, primary views with a parent included; a qualified `form_view_ref`
/// overrides; an unqualified, missing, other-model or other-type ref gives the default and says
/// why (BUG-073 for the last two).
#[tokio::test]
async fn default_view_selection_and_view_ref() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let form = |name: &str, priority: i32| {
        vec![
            ("name", s(name)),
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
            ("priority", Cell::Int(priority)),
        ]
    };
    let view1 = create(&mut tx, &form("p10", 10)).await.unwrap();
    let view2 = create(&mut tx, &form("p5", 5)).await.unwrap();
    let form_type = ViewType::new(&tx, "form").unwrap();
    assert_eq!(
        default_view(&mut tx, Some(VIEW), &form_type).await.unwrap(),
        Some(ViewId::new(view2).unwrap())
    );
    let view3 = create(
        &mut tx,
        &[
            ("name", s("p1 on p10")),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(view1)),
            ("mode", s("primary")),
            ("priority", Cell::Int(1)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await
    .unwrap();
    let default = ViewId::new(view3).unwrap();
    assert_eq!(
        default_view(&mut tx, Some(VIEW), &form_type).await.unwrap(),
        Some(default)
    );
    // an inactive or extension view is never the default
    let ext = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(view2)),
            ("priority", Cell::Int(0)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(mode(&mut tx, ext).await, "extension");
    write(&mut tx, &[view3], &[("active", Cell::Bool(false))])
        .await
        .unwrap();
    assert_eq!(
        default_view(&mut tx, Some(VIEW), &form_type).await.unwrap(),
        Some(ViewId::new(view2).unwrap())
    );
    write(&mut tx, &[view3], &[("active", Cell::Bool(true))])
        .await
        .unwrap();

    bind(&mut tx, VIEW, "base.test_views_test_view_ref", view1).await;
    let pick = async |tx: &mut Transaction, value: &str| {
        let context = Context::from_pairs([("form_view_ref", Value::str(value))]);
        let req = ViewRequest::new(VIEW, &form_type).context(&context);
        select_view(tx, &req).await.unwrap()
    };
    let picked = pick(&mut tx, "base.test_views_test_view_ref").await;
    assert_eq!(picked.id(), ViewId::new(view1).unwrap());
    assert!(picked.notices().is_empty());

    let picked = pick(&mut tx, "test_views_test_view_ref").await;
    assert_eq!(picked.id(), default);
    assert_eq!(
        picked.notices(),
        [ViewNotice::RefNotQualified {
            key: "form_view_ref".into(),
            value: "test_views_test_view_ref".into(),
            problem: "has no module part",
        }]
    );
    // the parse reason is kept (review F-6)
    assert_eq!(
        picked.notices()[0].to_string(),
        "'form_view_ref' requires a fully-qualified external id (got: \
         'test_views_test_view_ref', which has no module part); the default view is used"
    );
    let picked = pick(&mut tx, "base.nope").await;
    assert_eq!(picked.id(), default);
    assert_eq!(
        picked.notices(),
        [ViewNotice::RefNotFound {
            key: "form_view_ref".into(),
            xmlid: QualifiedXmlId::parse("base.nope").unwrap(),
        }]
    );
    let groups_form = create(
        &mut tx,
        &[
            ("model", s("res.groups")),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    bind(&mut tx, VIEW, "base.groups_form", groups_form).await;
    let picked = pick(&mut tx, "base.groups_form").await;
    assert_eq!(picked.id(), default);
    assert_eq!(
        picked.notices(),
        [ViewNotice::RefOtherModel {
            key: "form_view_ref".into(),
            xmlid: QualifiedXmlId::parse("base.groups_form").unwrap(),
            model: Some("res.groups".into()),
        }]
    );
    assert_eq!(
        picked.notices()[0].to_string(),
        "'form_view_ref': view 'base.groups_form' is a view of model 'res.groups'; the default \
         view is used"
    );
    let list = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<list><field name=\"name\"/></list>")),
        ],
    )
    .await
    .unwrap();
    bind(&mut tx, VIEW, "base.view_list", list).await;
    let picked = pick(&mut tx, "base.view_list").await;
    assert_eq!(picked.id(), default);
    assert_eq!(
        picked.notices(),
        [ViewNotice::RefOtherType {
            key: "form_view_ref".into(),
            xmlid: QualifiedXmlId::parse("base.view_list").unwrap(),
            view_type: Some("list".into()),
        }]
    );
    // a given view is used when it is of the request's model and type (review A-T5); one of
    // another type or model, or one that does not exist, is an error
    let req = ViewRequest::new(VIEW, &form_type).view_id(ViewId::new(view1).unwrap());
    assert_eq!(
        select_view(&mut tx, &req).await.unwrap().id(),
        ViewId::new(view1).unwrap()
    );
    let req = ViewRequest::new(VIEW, &form_type).view_id(ViewId::new(list).unwrap());
    let err = select_view(&mut tx, &req).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("View {list} is a view of type 'list', not 'form'")
    );
    let req = ViewRequest::new(VIEW, &form_type).view_id(ViewId::new(groups_form).unwrap());
    let err = select_view(&mut tx, &req).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("View {groups_form} is a view of model 'res.groups', not of 'ir.ui.view'")
    );
    let req = ViewRequest::new(VIEW, &form_type).view_id(ViewId::new(list + 1000).unwrap());
    let err = select_view(&mut tx, &req).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.to_string(),
        format!(
            "Record does not exist or has been deleted.\n(Record: ir.ui.view({}))",
            list + 1000
        )
    );
    // no view at all is an error naming the task
    let kanban = ViewType::new(&tx, "kanban").unwrap();
    let err = select_view(&mut tx, &ViewRequest::new(VIEW, &kanban))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.to_string(),
        "No default view of type 'kanban' could be found! (generated default views come with \
         a later release)"
    );
    // The web client's Group By menu reads "no search view" from this sentence's prefix
    // (`groupByCatalogue` in addons/web/static/src/client/app.ts); changing it breaks lists.
    let search = ViewType::new(&tx, "search").unwrap();
    let err = select_view(
        &mut tx,
        &ViewRequest::new("test.model_without_a_search_view", &search),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(
        err.to_string()
            .starts_with("No default view of type 'search' could be found!"),
        "{err}"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_view_finds_views_without_a_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let qweb = ViewType::new(&tx, "qweb").unwrap();
    let template = |priority: i32| {
        vec![
            ("priority", Cell::Int(priority)),
            ("mode", s("primary")),
            ("arch", s("<qweb/>")),
        ]
    };
    let view1 = create(&mut tx, &template(10)).await.unwrap();
    let view2 = create(&mut tx, &template(1)).await.unwrap();
    assert_eq!(text_field(&mut tx, view2, "model").await, None);
    assert_eq!(
        default_view(&mut tx, None, &qweb).await.unwrap(),
        Some(ViewId::new(view2).unwrap())
    );
    // testDefaultViewPrimary: priorities 10, 5 and a primary child of view1 at 1
    write(&mut tx, &[view2], &[("priority", Cell::Int(5))])
        .await
        .unwrap();
    let mut child = template(1);
    child.push(("inherit_id", Cell::Id(view1)));
    let view3 = create(&mut tx, &child).await.unwrap();
    assert_eq!(mode(&mut tx, view3).await, "primary");
    assert_eq!(
        default_view(&mut tx, None, &qweb).await.unwrap(),
        Some(ViewId::new(view3).unwrap())
    );
    // a model's lookup does not reach the templates, nor the templates' lookup a model's views
    assert_eq!(
        default_view(&mut tx, Some(VIEW), &qweb).await.unwrap(),
        None
    );
    create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    let form = ViewType::new(&tx, "form").unwrap();
    assert_eq!(default_view(&mut tx, None, &form).await.unwrap(), None);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A view type the selection does not have, a ref that is not text or too long: errors.
#[tokio::test]
async fn view_selection_refuses_bad_input() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let err = ViewType::new(&tx, "gantt").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // a request's type is not an arch's root tag: its own message (review A-T12)
    assert_eq!(
        err.to_string(),
        "Unknown view type 'gantt'. Allowed types are: list, form, graph, pivot, calendar, \
         kanban, search, card, qweb"
    );
    let form = ViewType::new(&tx, "form").unwrap();
    let context = Context::from_pairs([("form_view_ref", Value::Int(3))]);
    let err = select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(&context))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "context['form_view_ref'] must be the external id of a view, not Int(3)"
    );
    let long = format!("base.{}", "x".repeat(260));
    let context = Context::from_pairs([("form_view_ref", Value::str(long.as_str()))]);
    let err = select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(&context))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "context['form_view_ref'] is 265 bytes, more than the 256 allowed"
    );
    for id in [0, -1] {
        let err = ViewId::new(id).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            format!("{id} is not an ir.ui.view record id")
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

const E4_MISSING: &str = "Element '<e4>' cannot be located in parent view";

/// `test_check_primary_when_update_siblins_inherited_tree`: P1 with E1..E5 and an inactive
/// primary P2 on E2 replacing `<e4>` from E4.
#[tokio::test]
async fn sibling_primary_revalidated() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p1 = create(
        &mut tx,
        &[
            ("name", s("test_view_p1")),
            ("type", s("qweb")),
            ("key", s("website.test_view_p1")),
            ("arch_db", s("<div><p1/></div>")),
        ],
    )
    .await
    .unwrap();
    let ext = |n: u8, parent: i64, arch: String| {
        vec![
            ("name", s(&format!("test_view_e{n}"))),
            ("mode", s("extension")),
            ("inherit_id", Cell::Id(parent)),
            ("arch_db", s(&arch)),
            ("key", s(&format!("website.test_view_e{n}"))),
        ]
    };
    let inside = |n: u8| format!("<div position=\"inside\"><e{n}/></div>");
    let e1 = create(&mut tx, &ext(1, p1, inside(1))).await.unwrap();
    let e2 = create(&mut tx, &ext(2, p1, inside(2))).await.unwrap();
    let e3 = create(&mut tx, &ext(3, p1, inside(3))).await.unwrap();
    let e4 = create(&mut tx, &ext(4, p1, inside(4))).await.unwrap();
    let p2 = create(
        &mut tx,
        &[
            ("name", s("test_view_p2")),
            ("mode", s("primary")),
            ("inherit_id", Cell::Id(e2)),
            ("arch_db", s("<e4 position=\"replace\"><p2/></e4>")),
            ("key", s("website.test_view_p2")),
            ("active", Cell::Bool(false)),
        ],
    )
    .await
    .unwrap();
    let e5 = create(&mut tx, &ext(5, p1, inside(5))).await.unwrap();
    let all = [p1, e1, e2, e3, e4, p2, e5];
    assert_eq!(
        combined(&mut tx, &all, p1).await,
        "<div><p1/><e1/><e2/><e3/><e4/><e5/></div>"
    );
    write(&mut tx, &[e4], &[("active", Cell::Bool(false))])
        .await
        .unwrap();
    assert_eq!(
        combined(&mut tx, &all, p1).await,
        "<div><p1/><e1/><e2/><e3/><e5/></div>"
    );

    // P2 cannot be activated without E4
    let err = try_write(&mut tx, &[p2], &[("active", Cell::Bool(true))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("Error while validating view test_view_p2:\n{E4_MISSING}")
    );
    assert_eq!(context_of(&err, "name"), Ok(Value::str("test_view_p2")));
    let ViewRecordError::Invalid { view, source, .. } = record_error(&err) else {
        panic!("{err:?}")
    };
    assert_eq!(view.id(), p2);
    assert!(matches!(
        &**source,
        InvalidView::Combine(ViewError::InView { view_id, source, .. })
            if *view_id == p2 && **source == ViewError::TargetNotFound("<e4>".into())
    ));

    write(&mut tx, &[e4], &[("active", Cell::Bool(true))])
        .await
        .unwrap();
    write(&mut tx, &[p2], &[("active", Cell::Bool(true))])
        .await
        .unwrap();
    assert_eq!(
        combined(&mut tx, &all, p1).await,
        "<div><p1/><e1/><e2/><e3/><e4/><e5/></div>"
    );
    assert_eq!(
        combined(&mut tx, &all, p2).await,
        "<div><p1/><e1/><e2/><e3/><p2/><e5/></div>"
    );

    // E4 cannot be deactivated while P2 needs it: the error names P2, the context E4
    let err = try_write(&mut tx, &[e4], &[("active", Cell::Bool(false))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("Error while validating view test_view_p2:\n{E4_MISSING}")
    );
    assert_eq!(context_of(&err, "name"), Ok(Value::str("test_view_e4")));
    assert_eq!(
        text_field(&mut tx, e4, "name").await.unwrap(),
        "test_view_e4"
    );

    // an extension of E2 replacing <e4> runs before E4 is applied
    let err = try_create(
        &mut tx,
        &ext(6, e2, "<e4 position=\"replace\"><e6/></e4>".to_owned()),
    )
    .await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("Error while validating view test_view_e6:\n{E4_MISSING}")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `test_invalid_field`, `test_view_root_node_matches_view_type`,
/// `test_10_raise_for_old_attributes`: full messages, the view's name in the context for the
/// `attrs` error too (BUG-071), the line of the faulty node, the typed issue as the source.
#[tokio::test]
async fn validation_errors_name_the_view() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let view =
        |name: &str, arch: &str| vec![("name", s(name)), ("model", s(VIEW)), ("arch", s(arch))];
    let cases = [
        (
            view(
                "v_field",
                "<form string=\"View\">\n    <field name=\"name\"/>\n    <field name=\"not_a_field\"/>\n</form>",
            ),
            3,
            "Field \"not_a_field\" does not exist in model \"ir.ui.view\"",
        ),
        (
            view("v_noname", "<form string=\"View\">\n    <field/>\n</form>"),
            2,
            "Field tag must have a \"name\" attribute defined",
        ),
        (
            {
                let mut v = view(
                    "v_root",
                    "<data>\n    <div>\n    </div>\n    <form>\n    </form>\n</data>",
                );
                v.push(("type", s("form")));
                v
            },
            1,
            "The root node of a form view should be a <form>, not a <data>",
        ),
        (
            view(
                "v_attrs",
                "<form string=\"View\">\n    <field name=\"name\"/>\n    <field name=\"model\"/>\n    \
                 <field name=\"inherit_id\" attrs=\"{'readonly': [('model', '=', 'ir.ui.view')]}\"/>\n</form>",
            ),
            4,
            "Since 17.0, the \"attrs\" and \"states\" attributes are no longer used.",
        ),
    ];
    for (pairs, line, message) in cases {
        let name = match &pairs[0].1 {
            Cell::Str(n) => n.to_string(),
            _ => unreachable!(),
        };
        let err = try_create(&mut tx, &pairs).await;
        assert_eq!(err.kind(), ErrorKind::Validation, "{name}");
        assert_eq!(
            err.to_string(),
            format!("Error while validating view {name}, arch line {line}:\n{message}")
        );
        assert_eq!(context_of(&err, "name"), Ok(Value::str(name.as_str())));
        assert_eq!(context_of(&err, "line"), Ok(Value::Int(line)));
        assert_eq!(context_of(&err, "view.model"), Ok(Value::str(VIEW)));
        let ViewRecordError::Invalid { source, .. } = record_error(&err) else {
            panic!("{err:?}")
        };
        let InvalidView::Arch(issue) = &**source else {
            panic!("{source:?}")
        };
        assert_eq!(issue.to_string(), message);
        assert_eq!(i64::from(issue.origin().line()), line);
    }
    // the root view's root tag gives the type; a valid form passes
    let ok = create(&mut tx, &view("foo", "<form>\n</form>"))
        .await
        .unwrap();
    assert_eq!(text_field(&mut tx, ok, "type").await.unwrap(), "form");
    assert_eq!(text_field(&mut tx, ok, "name").await.unwrap(), "foo");
    let unnamed = create(&mut tx, &[("model", s(VIEW)), ("arch", s("<list/>"))])
        .await
        .unwrap();
    assert_eq!(
        text_field(&mut tx, unnamed, "name").await.unwrap(),
        "ir.ui.view list"
    );
    // an arch with a tag that is no view type, no arch at all
    let err = try_create(&mut tx, &[("arch", s("<sheet/>"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Invalid view type: 'sheet'.\nYou might have used an invalid starting tag in the \
         architecture.\nAllowed types are: list, form, graph, pivot, calendar, kanban, search, \
         card, qweb"
    );
    let ViewRecordError::InvalidType { tag, allowed } = record_error(&err) else {
        panic!("{err:?}")
    };
    assert_eq!(tag, "sheet");
    assert_eq!(
        allowed,
        &[
            "list", "form", "graph", "pivot", "calendar", "kanban", "search", "card", "qweb"
        ]
    );
    let err = try_create(&mut tx, &[("model", s(VIEW))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.to_string(), "Missing view architecture.");
    for (arch, message) in [
        (
            "<sheet><div></sheet>",
            "Error while validating view ir.ui.view None:\nXML syntax error at line 1, column 13: \
             expected 'div' tag, not 'sheet' at 1:13",
        ),
        (
            "<form><div></form>",
            "Error while validating view ir.ui.view form:\nXML syntax error at line 1, column 12: \
             expected 'div' tag, not 'form' at 1:12",
        ),
    ] {
        let err = try_create(&mut tx, &[("model", s(VIEW)), ("arch", s(arch))]).await;
        assert_eq!(err.kind(), ErrorKind::Validation, "{arch}");
        let ViewRecordError::Invalid { source, .. } = record_error(&err) else {
            panic!("{arch}: {err:?}")
        };
        assert!(
            matches!(&**source, InvalidView::Combine(_)),
            "{arch}: {source:?}"
        );
        assert_eq!(err.to_string(), message, "{arch}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The distinct group references of one check are bounded before they are looked up (review
/// R-1/S-1): 65 elements naming 64 groups each are 4,160 references, over the 4,096 allowed.
#[tokio::test]
async fn a_check_naming_too_many_groups_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let body: String = (0..65)
        .map(|e| {
            let groups = (0..64)
                .map(|g| format!("m.g{e}_{g}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("<div groups=\"{groups}\"/>")
        })
        .collect();
    let arch = format!("<form>{body}</form>");
    let err = try_create(&mut tx, &[("model", s(VIEW)), ("arch", s(&arch))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view ir.ui.view form, arch line 1:\n\
         The view lists 4097 distinct groups, more than the 4096 allowed"
    );
    let ViewRecordError::Invalid { source, .. } = record_error(&err) else {
        panic!("{err:?}");
    };
    assert!(matches!(
        &**source,
        InvalidView::Arch(
            erp_sdk::views::validate::ArchIssue::TooManyGroupReferences {
                count: 4097,
                max: 4096,
                ..
            }
        )
    ));
    // 64 elements (4,096 references) pass
    let body: String = (0..64)
        .map(|e| {
            let groups = (0..64)
                .map(|g| format!("m.g{e}_{g}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("<div groups=\"{groups}\"/>")
        })
        .collect();
    create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s(&format!("<form>{body}</form>"))),
        ],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The bytes of the fetched trees are bounded in the fetch statement (review S-3): a tree whose
/// archs total more than `LimitKind::CatalogueBytes` is refused at the row that crosses it,
/// before the archs are sent or parsed. The 10 MiB archs are written with SQL (they are no valid
/// specs: the check refuses the tree before reading them).
#[tokio::test]
async fn a_tree_over_the_byte_limit_is_refused_before_it_is_fetched() {
    use erp_sdk::views::{LimitKind, ViewLimits};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root_arch = "<form/>";
    let root = create(&mut tx, &[("model", s(VIEW)), ("arch", s(root_arch))])
        .await
        .unwrap();
    let limits = ViewLimits::default();
    let arch_bytes = limits.get(LimitKind::ArchBytes);
    let max = limits.get(LimitKind::CatalogueBytes);
    // archs of the per-arch maximum, until the tree passes the limit
    let count = max / arch_bytes + 1;
    let inserted = tx
        .execute(
            "INSERT INTO ir_ui_view (name, model, type, mode, priority, active, inherit_id, arch_db) \
             SELECT 'big' || n, $1, 'form', 'extension', 16, true, $2, repeat('x', $3) \
               FROM generate_series(1, $4) AS n",
            &[
                &VIEW,
                &root,
                &i32::try_from(arch_bytes).unwrap(),
                &i32::try_from(count).unwrap(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(usize::try_from(inserted).unwrap(), count);
    let written = "<form></form>";
    let err = try_write(&mut tx, &[root], &[("arch", s(written))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    // the row that crosses is the last extension: the root's arch as written (flushed before
    // the fetch) plus every big arch
    let root_text: i64 = tx
        .query(
            "SELECT 2 * octet_length(name)::bigint + 2 * octet_length(model)::bigint +
         octet_length(type)::bigint + octet_length(mode)::bigint FROM ir_ui_view WHERE id = $1",
            &[&root],
        )
        .await
        .unwrap()[0]
        .get(0);
    let extension_text: usize = (1..=count)
        .map(|n| 2 * format!("big{n}").len() + 2 * VIEW.len() + "form".len() + "extension".len())
        .sum();
    let actual =
        written.len() + count * arch_bytes + usize::try_from(root_text).unwrap() + extension_text;
    assert_eq!(
        err.to_string(),
        format!(
            "the text of the inheritance trees this check reads (the trees of all its \
             written views together) is {actual} bytes or more, more than the {max} allowed"
        )
    );
    assert!(matches!(
        record_error(&err),
        ViewRecordError::Limit {
            limit: erp_sdk::views::record::RecordLimit::CatalogueBytes,
            actual: a,
            max: m,
        } if *a == actual && *m == max
    ));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// same warnings are also written to the log is not)
#[tokio::test]
async fn arch_warnings_are_posted_to_transaction_notice_channel() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.clear_notices();
    assert!(tx.notices().is_empty());

    // Creating a view with an x2many subview emits ArchWarning::SubViewNotValidated
    let arch_with_subview = r#"<form string="Partner">
    <field name="inherit_children_ids">
        <list>
            <field name="name"/>
        </list>
    </field>
</form>"#;
    let view_id = create(
        &mut tx,
        &[
            ("name", s("partner_form_with_subview")),
            ("model", s(VIEW)),
            ("arch", s(arch_with_subview)),
        ],
    )
    .await
    .unwrap();

    let notices = tx.take_notices();
    assert_eq!(notices.len(), 1, "expected 1 arch warning notice on create");
    assert_eq!(notices[0].kind(), NoticeKind::SubViewNotValidated);
    assert_eq!(notices[0].target(), "erp::views");
    assert!(
        notices[0]
            .message()
            .contains("x2many sub-view(s) are not validated until a later release"),
        "unexpected notice message: {}",
        notices[0].message()
    );
    assert!(
        notices[0].message().contains("partner_form_with_subview"),
        "notice should contain view name: {}",
        notices[0].message()
    );

    // After take_notices(), notices channel is empty
    assert!(tx.notices().is_empty());

    // Writing an arch with an x2many subview also posts a notice
    write(&mut tx, &[view_id], &[("arch", s(arch_with_subview))])
        .await
        .unwrap();

    let write_notices = tx.take_notices();
    assert_eq!(
        write_notices.len(),
        1,
        "expected 1 arch warning notice on write"
    );
    assert_eq!(write_notices[0].kind(), NoticeKind::SubViewNotValidated);
    assert_eq!(write_notices[0].target(), "erp::views");
    assert!(
        write_notices[0]
            .message()
            .contains("x2many sub-view(s) are not validated until a later release"),
        "unexpected notice message: {}",
        write_notices[0].message()
    );

    // Savepoint rollback test: notices emitted in a failed savepoint are rolled back
    let failing_pairs = vec![
        ("name", s("failing_view_with_subview")),
        ("model", s(VIEW)),
        ("arch", s(arch_with_subview)),
    ];
    let failed_res = tx
        .savepoint(async move |tx: &mut Transaction| {
            create(tx, &failing_pairs).await?;
            // Force a validation error to trigger savepoint rollback
            Err::<(), _>(erp_sdk::Error::validation("intentional rollback"))
        })
        .await;
    let failed = failed_res.unwrap_err();
    assert_eq!(failed.kind(), ErrorKind::Validation);
    assert_eq!(failed.message(), "intentional rollback");
    // Notices emitted in the rolled back savepoint must not leak into tx
    assert!(
        tx.notices().is_empty(),
        "notices from rolled-back savepoint must be discarded"
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

const SUBVIEW_ARCH: &str = r#"<form string="Partner">
    <field name="inherit_children_ids">
        <list>
            <field name="name"/>
        </list>
    </field>
</form>"#;

fn subview_pairs(name: &'static str) -> Vec<(&'static str, Cell)> {
    vec![
        ("name", s(name)),
        ("model", s(VIEW)),
        ("arch", s(SUBVIEW_ARCH)),
    ]
}

/// A release keeps the notices in the parent; a rollback to the savepoint drops only what was
/// posted inside it; `take_notices` inside a savepoint followed by a rollback leaves the channel
/// empty; a nested rollback to the outer savepoint drops the notices of both levels.
#[tokio::test]
async fn notices_follow_savepoint_release_and_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // a release keeps the savepoint's notices in the parent
    let kept = subview_pairs("released_view");
    tx.savepoint(async move |tx: &mut Transaction| create(tx, &kept).await)
        .await
        .unwrap();
    assert_eq!(tx.notices().len(), 1, "a release keeps the notices");
    assert_eq!(tx.notices()[0].kind(), NoticeKind::SubViewNotValidated);

    // a rollback drops only what was posted inside the savepoint
    let dropped = subview_pairs("rolled_back_view");
    let err = tx
        .savepoint(async move |tx: &mut Transaction| {
            create(tx, &dropped).await?;
            assert_eq!(tx.notices().len(), 2);
            Err::<(), _>(erp_sdk::Error::validation("inner failure"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "inner failure");
    assert_eq!(tx.notices().len(), 1, "only the earlier notice survives");

    // take_notices inside a savepoint, then a rollback to it: nothing comes back
    let inner = subview_pairs("taken_view");
    let err = tx
        .savepoint(async move |tx: &mut Transaction| {
            create(tx, &inner).await?;
            assert_eq!(tx.take_notices().len(), 2);
            Err::<(), _>(erp_sdk::Error::validation("after take"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "after take");
    assert!(tx.notices().is_empty(), "taken notices do not come back");

    // nested: the outer rollback drops the notices of both levels
    let outer = subview_pairs("outer_view");
    let nested = subview_pairs("nested_view");
    let err = tx
        .savepoint(async move |tx: &mut Transaction| {
            create(tx, &outer).await?;
            tx.savepoint(async move |tx: &mut Transaction| create(tx, &nested).await)
                .await?;
            assert_eq!(tx.notices().len(), 2, "the released inner level is kept");
            Err::<(), _>(erp_sdk::Error::validation("outer failure"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "outer failure");
    assert!(tx.notices().is_empty(), "both levels are dropped");

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_notice_channel_is_bounded_and_counts_what_it_refuses() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for i in 0..MAX_NOTICES_PER_TRANSACTION + 3 {
        tx.post_notice(Notice::new(NoticeKind::Other, "erp::test", format!("n{i}")));
    }
    assert_eq!(tx.notices().len(), MAX_NOTICES_PER_TRANSACTION);
    assert_eq!(tx.notices_overflowed(), 3);
    assert_eq!(tx.notices()[0].message(), "n0");

    // a rollback to a savepoint does not decrease the counter
    let err = tx
        .savepoint(async move |tx: &mut Transaction| {
            tx.post_notice(Notice::new(NoticeKind::Other, "erp::test", "refused"));
            Err::<(), _>(erp_sdk::Error::validation("rolled back"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "rolled back");
    assert_eq!(tx.notices_overflowed(), 4);

    // taking the notices resets the counter and makes room again
    assert_eq!(tx.take_notices().len(), MAX_NOTICES_PER_TRANSACTION);
    assert_eq!(tx.notices_overflowed(), 0);
    tx.post_notice(Notice::new(NoticeKind::Other, "erp::test", "again"));
    assert_eq!(tx.notices().len(), 1);
    // clear_notices resets the counter too (one notice is already held)
    // clear_notices resets the counter too
    for i in 0..MAX_NOTICES_PER_TRANSACTION {
        tx.post_notice(Notice::new(NoticeKind::Other, "erp::test", format!("m{i}")));
    }
    assert_eq!(tx.notices_overflowed(), 1);
    tx.clear_notices();
    assert_eq!(tx.notices_overflowed(), 0);
    assert!(tx.notices().is_empty());

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An earlier failed statement aborts the transaction: the commit rolls it back and the notices
/// go with it.
#[tokio::test]
async fn a_failed_commit_leaves_no_notices() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &subview_pairs("before_failure"))
        .await
        .unwrap();
    assert_eq!(tx.notices().len(), 1);
    let failed = tx.query("SELECT 1 / 0", &[]).await.unwrap_err();
    assert_eq!(failed.kind(), ErrorKind::Database);
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Database);
    assert!(
        commit
            .message()
            .starts_with("transaction rolled back: an earlier statement failed ("),
        "{}",
        commit.message()
    );
    assert!(
        commit.message().contains("division by zero"),
        "{}",
        commit.message()
    );
    assert!(tx.notices().is_empty(), "a failed commit leaves no notices");
    assert_eq!(tx.notices_overflowed(), 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A commit refused because an earlier create failed validation (the transaction is poisoned)
/// does not end the attempt: the notices stay until the rollback ends it.
#[tokio::test]
async fn a_refused_commit_keeps_notices_until_the_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &subview_pairs("before_refusal"))
        .await
        .unwrap();
    let bad = create(
        &mut tx,
        &[
            ("name", s("bad_view")),
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"no_such_field\"/></form>")),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(bad.kind(), ErrorKind::Validation);
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Validation);
    assert_eq!(commit.message(), bad.message());
    assert_eq!(
        tx.notices().len(),
        1,
        "the refused commit did not end the attempt"
    );
    tx.rollback().await.unwrap();
    assert!(tx.notices().is_empty(), "the rollback ended the attempt");
    h.cleanup().await.unwrap();
}

/// A created extension's own unknown field is
/// still refused, and a `replace` or `attributes` spec widens the create to the whole arch.
#[tokio::test]
async fn a_create_validates_only_what_the_created_views_wrote() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("name", s("root")),
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO ir_ui_view (name, model, type, mode, priority, active, inherit_id, arch_db) \
         VALUES ('stale', $1, 'form', 'extension', 16, true, $2, $3)",
        &[
            &VIEW,
            &root,
            &"<field name=\"name\" position=\"after\"><field name=\"gone\"/></field>",
        ],
    )
    .await
    .unwrap();
    let ext = |name: &str, arch: &str| {
        vec![
            ("name", s(name)),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            ("arch", s(arch)),
        ]
    };
    let new_arch = "<field name=\"name\" position=\"before\"><field name=\"model\"/></field>";
    let created = create(&mut tx, &ext("new", new_arch)).await.unwrap();
    let stale_message = "Error while validating view stale, arch line 1:\n\
                         Field \"gone\" does not exist in model \"ir.ui.view\"";
    // a write is validated whole: the stale sibling's field is raised, naming that view
    let err = try_write(&mut tx, &[created], &[("arch", s(new_arch))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.to_string(), stale_message);
    assert_eq!(context_of(&err, "name"), Ok(Value::str("new")));
    // the created view's own unknown field is raised
    let err = try_create(
        &mut tx,
        &ext(
            "own",
            "<field name=\"name\" position=\"before\"><field name=\"nope\"/></field>",
        ),
    )
    .await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view own, arch line 1:\n\
         Field \"nope\" does not exist in model \"ir.ui.view\""
    );
    // `attributes` and `replace` widen the create to the whole arch
    for (name, arch) in [
        (
            "attrs",
            "<field name=\"name\" position=\"attributes\"><attribute name=\"string\">N</attribute></field>",
        ),
        (
            "replace",
            "<field name=\"model\" position=\"replace\"><field name=\"model\"/></field>",
        ),
    ] {
        let err = try_create(&mut tx, &ext(name, arch)).await;
        assert_eq!(err.kind(), ErrorKind::Validation, "{name}");
        assert_eq!(err.to_string(), stale_message, "{name}");
        assert_eq!(context_of(&err, "name"), Ok(Value::str(name)));
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A root without arch is
/// refused when a check reaches it.
#[tokio::test]
async fn views_created_without_arch_are_not_checked() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("name", s("root")),
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    let archless = create(
        &mut tx,
        &[
            ("name", s("archless")),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(text_field(&mut tx, archless, "arch_db").await, None);
    assert_eq!(text_field(&mut tx, archless, "type").await.unwrap(), "form");
    // a sibling's create and a write of the archless view combine it as `<data/>`
    create(
        &mut tx,
        &[
            ("name", s("sibling")),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(root)),
            (
                "arch",
                s("<field name=\"name\" position=\"after\"><field name=\"model\"/></field>"),
            ),
        ],
    )
    .await
    .unwrap();
    write(&mut tx, &[archless], &[("active", Cell::Bool(false))])
        .await
        .unwrap();
    write(&mut tx, &[archless], &[("active", Cell::Bool(true))])
        .await
        .unwrap();
    // a root given only its type is created; a check that reaches it refuses its empty arch
    let typed = create(
        &mut tx,
        &[
            ("name", s("typed")),
            ("model", s(VIEW)),
            ("type", s("form")),
        ],
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[typed], &[("active", Cell::Bool(false))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view typed:\nEmpty arch definition"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A view whose parent chain loops is refused before anything is combined.
#[tokio::test]
async fn recursive_inheritance_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = create(&mut tx, &[("arch", s("<qweb/>"))]).await.unwrap();
    let b = create(
        &mut tx,
        &[("inherit_id", Cell::Id(a)), ("arch", s("<qweb/>"))],
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[a], &[("inherit_id", Cell::Id(b))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "You cannot create recursive inherited views."
    );
    assert_eq!(
        record_error(&err),
        &ViewRecordError::RecursiveInheritance {
            views: vec![a, b, a]
        }
    );
    let err = try_write(&mut tx, &[a], &[("inherit_id", Cell::Id(a))]).await;
    assert_eq!(
        err.to_string(),
        "You cannot create recursive inherited views."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

const GROUPS_ON_EXTENSION: &str = "Inherited view cannot have 'groups' defined on the record. \
                                   Use 'groups' attributes inside the view definition";

/// Before the fix every one of these writes passed.
#[tokio::test]
async fn writes_of_type_model_priority_and_mode_are_checked() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root =
        |name: &str, arch: &str| vec![("name", s(name)), ("model", s(VIEW)), ("arch", s(arch))];
    let child = |name: &str, parent: i64, arch: &str, extra: &[(&'static str, Cell)]| {
        let mut v = vec![
            ("name", s(name)),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(parent)),
            ("arch", s(arch)),
        ];
        v.extend(extra.iter().cloned());
        v
    };

    // type
    let typed = create(
        &mut tx,
        &root("typed", "<form><field name=\"name\"/></form>"),
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[typed], &[("type", s("list"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view typed, arch line 1:\nThe root node of a list view should be \
         a <list>, not a <form>"
    );

    // model
    let modelled = create(
        &mut tx,
        &root("modelled", "<form><field name=\"arch_fs\"/></form>"),
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[modelled], &[("model", s("res.groups"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view modelled, arch line 1:\nField \"arch_fs\" does not exist in \
         model \"res.groups\""
    );

    // priority: e2 builds on the field e1 adds
    let base = create(
        &mut tx,
        &root("base", "<form><field name=\"name\"/></form>"),
    )
    .await
    .unwrap();
    let e1 = create(
        &mut tx,
        &child(
            "e1",
            base,
            "<field name=\"name\" position=\"after\"><field name=\"model\"/></field>",
            &[("priority", Cell::Int(1))],
        ),
    )
    .await
    .unwrap();
    let e2 = create(
        &mut tx,
        &child(
            "e2",
            base,
            "<field name=\"model\" position=\"after\"><field name=\"key\"/></field>",
            &[("priority", Cell::Int(2))],
        ),
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[e2], &[("priority", Cell::Int(0))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view e2:\nElement '<field name=\"model\">' cannot be located in \
         parent view"
    );
    // an order that keeps e1 first passes
    write(&mut tx, &[e1], &[("priority", Cell::Int(0))])
        .await
        .unwrap();

    // mode: p, a primary view removing `name`, becomes an extension of the form q builds on
    let form = create(
        &mut tx,
        &root("form", "<form><field name=\"name\"/></form>"),
    )
    .await
    .unwrap();
    let p = create(
        &mut tx,
        &child(
            "p",
            form,
            "<field name=\"name\" position=\"replace\"/>",
            &[("mode", s("primary"))],
        ),
    )
    .await
    .unwrap();
    create(
        &mut tx,
        &child(
            "q",
            form,
            "<field name=\"name\" position=\"after\"><field name=\"model\"/></field>",
            &[("mode", s("primary"))],
        ),
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[p], &[("mode", s("extension"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view q:\nElement '<field name=\"name\">' cannot be located in \
         parent view"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `test_no_groups_for_inherited`.
#[tokio::test]
async fn no_groups_on_extension_records() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        &[
            ("name", s("test_no_groups_for_inherited_parent")),
            ("model", s(VIEW)),
            ("arch", s("<form></form>")),
        ],
    )
    .await
    .unwrap();
    let view = create(
        &mut tx,
        &[
            ("name", s("test_no_groups_for_inherited_child")),
            ("model", s(VIEW)),
            ("arch", s("<data></data>")),
            ("inherit_id", Cell::Id(parent)),
            ("mode", s("extension")),
        ],
    )
    .await
    .unwrap();
    // the harness has none of base's data: groups are created as during the base installation
    erp_testkit::base_install::set_base_installing(&mut tx, true).unwrap();
    let groups = tx.env().records("res.groups").unwrap();
    let group = groups
        .create(&mut tx, vec![vals(&[("name", s("G"))])])
        .await
        .unwrap()
        .ids()[0];
    let set_groups = |tx: &mut Transaction| {
        let views = tx.env().records(VIEW).unwrap().browse([view]).unwrap();
        let vals = RawVals::new().set_commands(
            "group_ids",
            [Command::set([RecId::from_real(group).unwrap()])],
        );
        (views, vals)
    };
    let (views, groups_vals) = set_groups(&mut tx);
    let err = tx
        .savepoint(async move |tx: &mut Transaction| views.write(tx, groups_vals).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.to_string(), GROUPS_ON_EXTENSION);
    assert_eq!(record_error(&err), &ViewRecordError::GroupsOnExtension);

    write(&mut tx, &[view], &[("mode", s("primary"))])
        .await
        .unwrap();
    let (views, groups_vals) = set_groups(&mut tx);
    views.write(&mut tx, groups_vals).await.unwrap();

    let err = try_write(&mut tx, &[view], &[("mode", s("extension"))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.to_string(), GROUPS_ON_EXTENSION);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Two addons extend one view of a third without editing it (the role rule: every extension
/// mechanism is tested with two addons on one view): `test_ext_b` and `test_ext_c` each load an
/// extension of `test_ext_a`'s book form through the data loader, by priority then id; a broken
/// extension names its data file and line.
#[tokio::test]
async fn two_addons_extend_one_view() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_test_ext_a::ADDON,
        &erp_addon_test_ext_b::ADDON,
        &erp_addon_test_ext_c::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let load = async |session: &mut LoadSession, tx: &mut Transaction, module: &str, xml: &str| {
        let file = FileContext::new(module, "data/views.xml", ConvertMode::Init);
        let mut ops = XmlDataReader::new(xml, "data/views.xml", module, false);
        session.load_file(tx, &file, &mut ops).await
    };
    load(
        &mut session,
        &mut tx,
        "test_ext_a",
        r#"<odoo><record id="view_book_form" model="ir.ui.view">
  <field name="name">test_ext.book.form</field>
  <field name="model">test_ext.book</field>
  <field name="arch" type="xml"><form><field name="name"/></form></field>
</record></odoo>"#,
    )
    .await
    .unwrap();
    let ext = |priority: u8, field: &str| {
        format!(
            r#"<odoo><record id="view_book_form_{field}" model="ir.ui.view">
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="test_ext_a.view_book_form"/>
  <field name="priority">{priority}</field>
  <field name="arch" type="xml"><field name="name" position="after"><field name="{field}"/></field></field>
</record></odoo>"#
        )
    };
    load(&mut session, &mut tx, "test_ext_b", &ext(20, "pages"))
        .await
        .unwrap();
    load(&mut session, &mut tx, "test_ext_c", &ext(10, "rating"))
        .await
        .unwrap();
    let base = xid_id(&mut tx, "test_ext_a.view_book_form").await;
    let b = xid_id(&mut tx, "test_ext_b.view_book_form_pages").await;
    let c = xid_id(&mut tx, "test_ext_c.view_book_form_rating").await;
    // c (priority 10) applies first and inserts after name; b (20) then inserts right after name
    assert_eq!(
        combined(&mut tx, &[base, b, c], base).await,
        "<form><field name=\"name\"/><field name=\"pages\"/><field name=\"rating\"/></form>"
    );
    assert_eq!(
        text_field(&mut tx, b, "arch_fs").await.unwrap(),
        "test_ext_b/data/views.xml"
    );
    let form = ViewType::new(&tx, "form").unwrap();
    assert_eq!(
        default_view(&mut tx, Some("test_ext.book"), &form)
            .await
            .unwrap(),
        Some(ViewId::new(base).unwrap())
    );
    // a third extension naming a field no addon added fails with its file and line
    let err = load(
        &mut session,
        &mut tx,
        "test_ext_c",
        r#"<odoo><record id="view_book_form_bad" model="ir.ui.view">
  <field name="name">bad</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="test_ext_a.view_book_form"/>
  <field name="arch" type="xml"><form position="inside">
    <field name="no_such_field"/>
  </form></field>
</record></odoo>"#,
    )
    .await
    .unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(orm) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(orm.kind(), ErrorKind::Validation);
    // the external id is written after the record, so the error has none yet; the arch starts
    // with the `<?xml ...?>` line the loader writes
    assert_eq!(
        orm.to_string(),
        "Error while validating view bad in test_ext_c/data/views.xml, arch line 3:\nField \
         \"no_such_field\" does not exist in model \"test_ext.book\""
    );
    assert_eq!(
        context_of(orm, "file"),
        Ok(Value::str("test_ext_c/data/views.xml"))
    );
    assert_eq!(context_of(orm, "xmlid"), Ok(Value::False));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Uninstall: a view copied from the removed module's views is
/// deleted with the views built on it (the `force_delete` cascade of the view model,
/// ); `inherit_id` would refuse the copy alone.
#[tokio::test]
async fn copied_views_and_their_children_are_removed_at_uninstall() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let copy = create(
        &mut tx,
        &[
            ("key", s("m1.copy")),
            ("type", s("qweb")),
            ("arch", s("<t><p/></t>")),
        ],
    )
    .await
    .unwrap();
    let child = create(
        &mut tx,
        &[
            ("key", s("other.child")),
            ("inherit_id", Cell::Id(copy)),
            ("arch", s("<p position=\"inside\"><b/></p>")),
            ("active", Cell::Bool(false)),
        ],
    )
    .await
    .unwrap();
    let keep = create(
        &mut tx,
        &[
            ("key", s("other.keep")),
            ("type", s("qweb")),
            ("arch", s("<t/>")),
        ],
    )
    .await
    .unwrap();
    // without the cascade the copy alone cannot go
    let err = tx
        .savepoint(async |tx: &mut Transaction| {
            tx.env().records(VIEW)?.browse([copy])?.unlink(tx).await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    let report = erp_data::load::purge_module_data(&mut tx, &["m1"])
        .await
        .unwrap();
    // the copy and the view built on it (review A-S7)
    assert_eq!(report.copied_views_removed, 2);
    assert_eq!(report.copied_views_kept, 0);
    let views = tx
        .env()
        .records(VIEW)
        .unwrap()
        .browse([copy, child, keep])
        .unwrap();
    assert_eq!(views.exists(&mut tx).await.unwrap().ids(), [keep]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The `force_delete` cascade collects the whole subtree once and deletes it in one unlink,
/// children first (review S-5): a chain three levels deep goes with its root, and a cycle in
/// `inherit_id` (written by raw SQL: `check_views` refuses one) ends. Before the fix each level
/// called `unlink` on its children again, so the cycle recursed without end (not run: it would
/// not terminate).
#[tokio::test]
async fn the_force_delete_cascade_takes_a_deep_chain_and_ends_on_a_cycle() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        &[
            ("key", s("m1.root")),
            ("type", s("qweb")),
            ("arch", s("<t><p/></t>")),
        ],
    )
    .await
    .unwrap();
    let mut chain = vec![root];
    for _ in 0..3 {
        let parent = *chain.last().unwrap();
        let child = create(
            &mut tx,
            &[
                ("inherit_id", Cell::Id(parent)),
                ("arch", s("<p position=\"inside\"><b/></p>")),
            ],
        )
        .await
        .unwrap();
        chain.push(child);
    }
    async fn forced(tx: &mut Transaction, ids: &[i64]) -> erp_sdk::Result<()> {
        tx.env()
            .records(VIEW)?
            .with_context([("force_delete", Value::Bool(true))])
            .browse(ids.iter().copied())?
            .unlink(tx)
            .await
    }
    forced(&mut tx, &[root]).await.unwrap();
    let views = tx.env().records(VIEW).unwrap().browse(chain).unwrap();
    assert!(views.exists(&mut tx).await.unwrap().is_empty());

    // a two-view cycle, made by raw SQL
    let a = create(
        &mut tx,
        &[
            ("key", s("m1.a")),
            ("type", s("qweb")),
            ("arch", s("<t><p/></t>")),
        ],
    )
    .await
    .unwrap();
    let b = create(
        &mut tx,
        &[
            ("inherit_id", Cell::Id(a)),
            ("arch", s("<p position=\"inside\"><b/></p>")),
        ],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE ir_ui_view SET inherit_id = $1 WHERE id = $2",
        &[&b, &a],
    )
    .await
    .unwrap();
    tx.invalidate_model(VIEW, None).await.unwrap();
    forced(&mut tx, &[a]).await.unwrap();
    let views = tx.env().records(VIEW).unwrap().browse([a, b]).unwrap();
    assert!(views.exists(&mut tx).await.unwrap().is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn book_harness() -> erp_testkit::HarnessBuilder {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_test_ext_a::ADDON,
        &erp_addon_test_ext_b::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
}

async fn load_views(
    session: &mut LoadSession,
    tx: &mut Transaction,
    module: &str,
    mode: ConvertMode,
    xml: &str,
) -> Result<(), erp_data::load::LoadError> {
    let file = FileContext::new(module, "data/views.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/views.xml", module, false);
    session.load_file(tx, &file, &mut ops).await
}

fn book_form(fields: &[&str]) -> String {
    let body: String = fields
        .iter()
        .map(|f| format!("<field name=\"{f}\"/>"))
        .collect();
    format!(
        r#"<odoo><record id="view_book_form" model="ir.ui.view">
  <field name="name">book.form</field>
  <field name="model">test_ext.book</field>
  <field name="arch" type="xml"><form>{body}</form></field>
</record></odoo>"#
    )
}

/// `test_ext_b`'s primary view on `test_ext_a`'s form, under `noupdate`.
fn book_form_b(anchor: &str) -> String {
    format!(
        r#"<odoo noupdate="1"><record id="view_book_form_b" model="ir.ui.view">
  <field name="name">book.form.b</field>
  <field name="model">test_ext.book</field>
  <field name="mode">primary</field>
  <field name="inherit_id" ref="test_ext_a.view_book_form"/>
  <field name="arch" type="xml"><field name="{anchor}" position="after"><field name="isbn"/></field></field>
</record></odoo>"#
    )
}

/// In a module operation, the sibling primary views of modules whose data has not loaded yet are
/// not combined when a view changes: they still hold their archs of before the upgrade.
/// At the end of a module's upgrade its `noupdate` views, whose
/// writes were skipped, are checked (`_validate_module_views`).
#[tokio::test]
async fn upgrades_skip_views_not_loaded_yet_and_check_noupdate_views() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name", "pages"]),
    )
    .await
    .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Init,
        &book_form_b("pages"),
    )
    .await
    .unwrap();
    let b = xid_id(&mut tx, "test_ext_b.view_book_form_b").await;
    tx.commit().await.unwrap();

    // without an operation that names test_ext_b as pending, a's new arch breaks b's view
    let update = book_form(&["name"]);
    let mut tx = h.begin().await.unwrap();
    let mut lone = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let err = load_views(
        &mut lone,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &update,
    )
    .await
    .unwrap_err();
    tx.rollback().await.unwrap();
    let erp_data::load::LoadErrorKind::Orm(err) = err.kind() else {
        panic!("{err:?}")
    };
    let pages_missing = "Element '<field name=\"pages\">' cannot be located in parent view";
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!(
            "Error while validating view book.form.b (test_ext_b.view_book_form_b) in \
             test_ext_b/data/views.xml:\n{pages_missing}"
        )
    );

    let mut tx = h.begin().await.unwrap();
    // the upgrade of a then b: b's view is skipped while b is pending
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session
        .begin_operation(["test_ext_a", "test_ext_b"])
        .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &update,
    )
    .await
    .unwrap();
    assert_eq!(
        session.finish_module("test_ext_a").unwrap(),
        ["view_book_form"]
    );
    // b's record is `noupdate`: its write is skipped, so nothing checked it yet
    load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Update,
        &book_form_b("name"),
    )
    .await
    .unwrap();
    let names = session.finish_module("test_ext_b").unwrap();
    assert_eq!(names, ["view_book_form_b"]);
    let views = tx.env().records(VIEW).unwrap();
    let err = views
        .call::<(String, Vec<String>, Vec<String>), ()>(
            &mut tx,
            "_validate_module_views",
            ("test_ext_b".to_owned(), names, Vec::new()),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!(
            "Error while validating view book.form.b (test_ext_b.view_book_form_b) in \
             test_ext_b/data/views.xml:\n{pages_missing}"
        )
    );
    let ViewRecordError::Invalid { view, .. } = record_error(&err) else {
        panic!("{err:?}")
    };
    assert_eq!(view.id(), b);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Called as the module system
/// called it before the fix (no pending module), the check combines b's stale view and fails.
#[tokio::test]
async fn the_noupdate_check_skips_the_views_of_pending_modules() {
    let h = book_harness().build().await.unwrap();
    let a_views = |fields: &[&str]| {
        let body: String = fields
            .iter()
            .map(|f| format!("<field name=\"{f}\"/>"))
            .collect();
        format!(
            r#"<odoo><record id="view_book_form" model="ir.ui.view">
  <field name="name">book.form</field>
  <field name="model">test_ext.book</field>
  <field name="arch" type="xml"><form>{body}</form></field>
</record>
<data noupdate="1"><record id="view_book_ext" model="ir.ui.view">
  <field name="name">book.ext</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch" type="xml"><field name="name" position="after"><field name="isbn"/></field></field>
</record></data></odoo>"#
        )
    };
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &a_views(&["name", "pages"]),
    )
    .await
    .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Init,
        &book_form_b("pages"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session
        .begin_operation(["test_ext_a", "test_ext_b"])
        .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &a_views(&["name"]),
    )
    .await
    .unwrap();
    let names = session.finish_module("test_ext_a").unwrap();
    assert_eq!(names, ["view_book_ext", "view_book_form"]);
    let views = tx.env().records(VIEW).unwrap();
    let validate = async |tx: &mut Transaction, pending: Vec<String>| {
        let names = names.clone();
        let views = views.clone();
        tx.savepoint(async move |tx: &mut Transaction| {
            views
                .call::<(String, Vec<String>, Vec<String>), ()>(
                    tx,
                    "_validate_module_views",
                    ("test_ext_a".to_owned(), names, pending),
                )
                .await
        })
        .await
    };
    validate(&mut tx, vec!["test_ext_b".to_owned()])
        .await
        .unwrap();
    // without the pending module, b's view of before its upgrade is combined
    let err = validate(&mut tx, Vec::new()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view book.form.b (test_ext_b.view_book_form_b) in \
         test_ext_b/data/views.xml:\nElement '<field name=\"pages\">' cannot be located in \
         parent view"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Before the fix the load state was built at the window's start, so the
/// skipped view was not combined and the load passed.
#[tokio::test]
async fn a_skipped_noupdate_view_counts_as_loaded_in_its_window() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name", "pages"]),
    )
    .await
    .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Init,
        &book_form_b("pages"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session
        .begin_operation(["test_ext_a", "test_ext_b"])
        .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &book_form(&["name"]),
    )
    .await
    .unwrap();
    session.finish_module("test_ext_a").unwrap();
    // one window (text archs): b's view is skipped, then a new extension is created
    let window = r#"<odoo noupdate="1"><record id="view_book_form_b" model="ir.ui.view">
  <field name="name">book.form.b</field>
  <field name="model">test_ext.book</field>
  <field name="mode">primary</field>
  <field name="inherit_id" ref="test_ext_a.view_book_form"/>
  <field name="arch">&lt;field name="pages" position="after"&gt;&lt;field name="isbn"/&gt;&lt;/field&gt;</field>
</record>
<record id="view_book_form_c" model="ir.ui.view">
  <field name="name">book.form.c</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="test_ext_a.view_book_form"/>
  <field name="arch">&lt;field name="name" position="after"&gt;&lt;field name="isbn"/&gt;&lt;/field&gt;</field>
</record></odoo>"#;
    let err = load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Update,
        window,
    )
    .await
    .unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(err) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view book.form.b (test_ext_b.view_book_form_b) in \
         test_ext_b/data/views.xml:\nElement '<field name=\"pages\">' cannot be located in \
         parent view"
    );
    // the error is about the written view, book.form.c (BUG-071)
    assert_eq!(context_of(err, "name"), Ok(Value::str("book.form.c")));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- review round 1 --------------------------------------------

#[tokio::test]
async fn archived_views_are_checked_on_their_own() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let err = try_create(
        &mut tx,
        &[
            ("name", s("broken")),
            ("model", s(VIEW)),
            ("type", s("form")),
            ("active", Cell::Bool(false)),
            ("arch", s("<form><field name=\"name\"></form>")),
        ],
    )
    .await;
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(
        err.to_string()
            .starts_with("Error while validating view broken:\n"),
        "{err}"
    );
    let ViewRecordError::Invalid { source, .. } = record_error(&err) else {
        panic!("{err:?}")
    };
    assert!(matches!(**source, InvalidView::Combine(_)), "{source:?}");

    let err = try_create(
        &mut tx,
        &[
            ("name", s("archived form")),
            ("model", s(VIEW)),
            ("active", Cell::Bool(false)),
            ("arch", s("<form>\n<field name=\"not_a_field\"/></form>")),
        ],
    )
    .await;
    assert_eq!(
        err.to_string(),
        "Error while validating view archived form, arch line 2:\nField \"not_a_field\" does \
         not exist in model \"ir.ui.view\""
    );

    let base = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    // an archived extension whose spec would not apply is only parsed
    let ext = create(
        &mut tx,
        &[
            ("name", s("archived ext")),
            ("model", s(VIEW)),
            ("inherit_id", Cell::Id(base)),
            ("active", Cell::Bool(false)),
            ("arch", s("<field name=\"nowhere\" position=\"after\"/>")),
        ],
    )
    .await
    .unwrap();
    let err = try_write(&mut tx, &[ext], &[("arch", s("<field name=\"name\""))]).await;
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(
        err.to_string()
            .starts_with("Error while validating view archived ext:\n"),
        "{err}"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A view written with `install_module` in its context but without the loader's load state is a
/// programming error, never "nothing loaded" (review A-S2).
#[tokio::test]
async fn install_module_without_the_load_state_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let views = tx.env().records(VIEW).unwrap().with_context([
        ("install_mode", Value::Bool(true)),
        ("install_module", Value::str("m")),
        ("install_filename", Value::str("m/data/views.xml")),
    ]);
    let err = tx
        .savepoint(async |tx: &mut Transaction| {
            views
                .create(
                    tx,
                    vec![vals(&[
                        ("model", s(VIEW)),
                        ("arch", s("<form><field name=\"name\"/></form>")),
                    ])],
                )
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert_eq!(
        err.to_string(),
        "the context names install_module 'm' without 'install_pending_modules': a write in a \
         data load must carry the loader's load state (erp_data::load::load_state)"
    );
    // a broken loader contract, which the demo savepoint does not absorb (review F-1)
    assert_eq!(
        err.condition(),
        Some(erp_sdk::Condition::InvariantViolation)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A data file that gives `arch_db` instead of `arch` records no file: only `arch` is a data
/// file's arch (review A2-R5).
#[tokio::test]
async fn a_loaded_arch_db_records_no_file() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let xml = r#"<odoo><record id="view_book_db" model="ir.ui.view">
  <field name="name">book.db</field>
  <field name="model">test_ext.book</field>
  <field name="arch_db">&lt;form&gt;&lt;field name="name"/&gt;&lt;/form&gt;</field>
</record></odoo>"#;
    load_views(&mut session, &mut tx, "test_ext_a", ConvertMode::Init, xml)
        .await
        .unwrap();
    let id = xid_id(&mut tx, "test_ext_a.view_book_db").await;
    assert_eq!(text_field(&mut tx, id, "arch_fs").await, None);
    let views = tx.env().records(VIEW).unwrap().browse([id]).unwrap();
    assert_eq!(
        views.cells(&mut tx, "arch_updated").await.unwrap()[0],
        Cell::Null
    );
    assert_eq!(
        text_field(&mut tx, id, "arch_db").await.unwrap(),
        "<form><field name=\"name\"/></form>"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A caller's `arch_fs` must name a module file, as the loader's does (review S-7): a path that
/// leaves its module is refused on create and on write; a module file and a cleared value pass.
#[tokio::test]
async fn a_written_arch_fs_must_name_a_module_file() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let form = || ("arch", s("<form/>"));
    let id = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            form(),
            ("arch_fs", s("base/views/a.xml")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        text_field(&mut tx, id, "arch_fs").await.unwrap(),
        "base/views/a.xml"
    );
    for bad in [
        "../../etc/x",
        "base/../etc/x",
        "/etc/x",
        "base",
        "base//x",
        "base/./x",
    ] {
        let message = format!(
            "ir.ui.view: arch_fs '{bad}' is not a module file: it must be '<module>/<path>', \
             without empty, '.' or '..' components"
        );
        let err = try_create(&mut tx, &[("model", s(VIEW)), form(), ("arch_fs", s(bad))]).await;
        assert_eq!(err.kind(), ErrorKind::Validation, "{bad}");
        assert_eq!(err.to_string(), message);
        let err = try_write(&mut tx, &[id], &[("arch_fs", s(bad))]).await;
        assert_eq!(err.kind(), ErrorKind::Validation, "{bad}");
        assert_eq!(err.to_string(), message);
    }
    assert_eq!(
        text_field(&mut tx, id, "arch_fs").await.unwrap(),
        "base/views/a.xml"
    );
    write(&mut tx, &[id], &[("arch_fs", Cell::Null)])
        .await
        .unwrap();
    assert_eq!(text_field(&mut tx, id, "arch_fs").await, None);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An `install_filename` that is not a file of `install_module` (`<module>/<path>`) is a
/// programming error, never stored as `arch_fs` (review A2-R2: one convention, checked).
#[tokio::test]
async fn an_install_filename_outside_the_module_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let refused = async |tx: &mut Transaction, module: Value, file: Value| -> erp_sdk::Error {
        let views = tx
            .env()
            .records(VIEW)
            .unwrap()
            .with_context([("install_module", module), ("install_filename", file)]);
        tx.savepoint(async |tx: &mut Transaction| {
            views
                .create(
                    tx,
                    vec![vals(&[
                        ("model", s(VIEW)),
                        ("arch", s("<form><field name=\"name\"/></form>")),
                    ])],
                )
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err()
    };
    // outside the module's directory, or a path with an empty, `.` or `..` component (review
    // A3-R3): never stored as `arch_fs`
    for (module, file) in [
        ("m", "data/views.xml"),
        ("m", "m/"),
        ("m", "mx/data/views.xml"),
        ("m", "m/../other/x.xml"),
        ("m", "m//x.xml"),
        ("m", "m/./x.xml"),
        ("m", "m/data/"),
        ("", "/x.xml"),
    ] {
        let err = refused(&mut tx, Value::str(module), Value::str(file)).await;
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert_eq!(
            err.to_string(),
            format!(
                "ir.ui.view: install_filename '{file}' is not a file of module '{module}': it \
                 must be '<module>/<path>', without empty, '.' or '..' components"
            )
        );
    }
    // a value that is not text (review A3-R6: its own message)
    let err = refused(&mut tx, Value::str("m"), Value::Int(3)).await;
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert_eq!(
        err.to_string(),
        "ir.ui.view: the data loader's context has install_module Some(Str(\"m\")) and \
         install_filename Some(Int(3)); both must be text"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_csv_loaded_view_records_its_file() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let path = "data/ir.ui.view.csv";
    let content = "id,name,model,arch\n\
                   view_book_csv,book.csv,test_ext.book,\"<form><field name=\"\"name\"\"/></form>\"\n";
    let erp_data::CsvImportResult::Loaded(import) =
        erp_data::parse_csv(content, path, "test_ext_a", ConvertMode::Init).unwrap()
    else {
        panic!("the file has rows");
    };
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let file = FileContext::new("test_ext_a", path, ConvertMode::Init);
    let mut ops = vec![Ok(import.into_operation(false))].into_iter();
    session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    let id = xid_id(&mut tx, "test_ext_a.view_book_csv").await;
    assert_eq!(
        text_field(&mut tx, id, "arch_fs").await.unwrap(),
        "test_ext_a/data/ir.ui.view.csv"
    );
    assert_eq!(
        text_field(&mut tx, id, "arch_db").await.unwrap(),
        "<form><field name=\"name\"/></form>"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A write that carries a load state (`install_module` and its lists) without `install_mode`
/// is refused: only the data loader sets the load state, and it always sets `install_mode`
/// (review A3-S1). A write of `active` (no `arch`, so the loader's file is not read) is the case
/// that passed before: a forged state could make the check skip the sibling views.
#[tokio::test]
async fn a_load_state_without_install_mode_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        &[
            ("model", s(VIEW)),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    let views = tx
        .env()
        .records(VIEW)
        .unwrap()
        .with_context([
            ("install_module", Value::str("m")),
            ("install_pending_modules", Value::list([Value::str("x")])),
            ("install_loaded_names", Value::list([])),
        ])
        .browse([id])
        .unwrap();
    let err = tx
        .savepoint(async |tx: &mut Transaction| {
            views
                .write(tx, vals(&[("active", Cell::Bool(false))]))
                .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert_eq!(
        err.to_string(),
        "the context names install_module 'm' without install_mode: the load state is the data \
         loader's, which always sets install_mode (erp_data::load::load_state)"
    );
    assert_eq!(
        err.condition(),
        Some(erp_sdk::Condition::InvariantViolation)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A user edits a view of a tree while an upgrade loads the tree's root (review A3-D2): the
/// upgrade, whose snapshot is older than the user's commit, fails with a serialization error
/// (`Concurrency`, `40001`) from the loader instead of committing a tree its check did not see
/// whole. The module system reports it as a concurrent change to retry
/// (`erp-modules` `a_concurrent_change_asks_for_a_retry`).
#[tokio::test]
async fn a_user_write_during_an_upgrade_fails_the_upgrade() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name", "pages"]),
    )
    .await
    .unwrap();
    let root = xid_id(&mut tx, "test_ext_a.view_book_form").await;
    let user_view = create(
        &mut tx,
        &[
            ("model", s("test_ext.book")),
            ("inherit_id", Cell::Id(root)),
            ("arch", s("<form position=\"inside\"/>")),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut user = h.begin().await.unwrap();
    let mut upgrade = h.begin().await.unwrap();
    user.query("SELECT 1", &[]).await.unwrap();
    upgrade.query("SELECT 1", &[]).await.unwrap();
    write(
        &mut user,
        &[user_view],
        &[("arch", s("<field name=\"pages\" position=\"after\"/>"))],
    )
    .await
    .unwrap();
    user.flush().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session.begin_operation(["test_ext_a"]).unwrap();
    let update = book_form(&["name"]);
    let (loaded, committed) = tokio::join!(
        load_views(
            &mut session,
            &mut upgrade,
            "test_ext_a",
            ConvertMode::Update,
            &update,
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            user.commit().await
        }
    );
    committed.unwrap();
    let err = loaded.expect_err("the upgrade must fail");
    let erp_data::load::LoadErrorKind::Orm(err) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(err.kind(), ErrorKind::Concurrency, "{err:?}");
    assert_eq!(err.sqlstate(), Some("40001"), "{err:?}");
    drop(upgrade);
    h.cleanup().await.unwrap();
}

/// A model whose `create` writes a view: a window of another model that writes views
/// (review A-S2).
mod view_maker {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_view_maker", depends = ["base", "test_ext_a"]);

    /// Creates an extension of `parent_view` with `arch` on `test_ext.book` with each record.
    #[model(
        name = "test_view_maker.maker",
        description = "View Maker",
        addon = "test_view_maker"
    )]
    pub struct Maker {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(integer)]
        pub parent_view: i32,
        #[field(text)]
        pub arch: Option<Arc<str>>,
    }

    #[methods(Maker, addon = "test_view_maker")]
    impl Maker {
        #[method(override)]
        async fn create(
            this: &RecordSet<Maker>,
            tx: &mut Transaction,
            vals_list: Vec<RawVals>,
            sup: Super,
        ) -> Result<Records> {
            let records = this.as_records();
            // the views are written with this window's context
            let views = records.env().records("ir.ui.view")?;
            let mut made = Vec::new();
            for vals in &vals_list {
                let (Some(Cell::Int(parent)), Some(Cell::Str(arch))) =
                    (vals.get("parent_view"), vals.get("arch"))
                else {
                    return Err(Error::validation("parent_view and arch are required"));
                };
                made.push(
                    RawVals::new()
                        .set_cell("model", Cell::Str(Arc::from("test_ext.book")))
                        .set_cell("inherit_id", Cell::Id(i64::from(*parent)))
                        .set_cell("arch", Cell::Str(Arc::clone(arch))),
                );
            }
            views.create(tx, made).await?;
            sup.call(tx, records, (vals_list,)).await
        }
    }
}

/// Every write of a data load that carries `install_module` carries the load state, also a view
/// written by another model's `create` (review A-S2): in the upgrade of a, c, then b, the view
/// c's maker writes on a's form skips b's view, which still holds its arch of before the upgrade.
/// Without the load state, b's view would be combined and the upgrade refused.
#[tokio::test]
async fn views_written_by_another_models_window_carry_the_load_state() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_test_ext_a::ADDON,
        &erp_addon_test_ext_b::ADDON,
        &erp_addon_test_ext_c::ADDON,
        &view_maker::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(model_data::UNLINK_HOOK)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name", "pages"]),
    )
    .await
    .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_b",
        ConvertMode::Init,
        &book_form_b("pages"),
    )
    .await
    .unwrap();

    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session
        .begin_operation(["test_ext_a", "test_view_maker", "test_ext_b"])
        .unwrap();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &book_form(&["name"]),
    )
    .await
    .unwrap();
    session.finish_module("test_ext_a").unwrap();
    let maker = r#"<odoo><record id="maker_1" model="test_view_maker.maker">
  <field name="name">rating after name</field>
  <field name="parent_view" eval="ref('test_ext_a.view_book_form')"/>
  <field name="arch" type="xml"><field name="name" position="after"><field name="rating"/></field></field>
</record></odoo>"#;
    load_views(
        &mut session,
        &mut tx,
        "test_view_maker",
        ConvertMode::Update,
        maker,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// S, written
/// just before in the same batch, is combined when E is checked, and its new spec on `pages`
/// fails the upgrade. Neither record refers to the other and the archs are text (an `type="xml"`
/// value may hold `%(xid)d` markers, so the loader writes the window before such a record): both
/// records are in one batch.
#[tokio::test]
async fn a_view_written_earlier_in_the_window_counts_as_loaded() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let esc = |xml: &str| {
        xml.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let file = |anchor: &str, ext: &str| {
        let s_arch = esc(&format!(
            "<field name=\"{anchor}\" position=\"after\"><field name=\"isbn\"/></field>"
        ));
        let e_arch = esc(ext);
        format!(
            r#"<odoo><record id="view_book_s" model="ir.ui.view">
  <field name="name">book.s</field>
  <field name="model">test_ext.book</field>
  <field name="mode">primary</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{s_arch}</field>
</record><record id="view_book_e" model="ir.ui.view">
  <field name="name">book.e</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{e_arch}</field>
</record></odoo>"#
        )
    };
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name", "pages"]),
    )
    .await
    .unwrap();
    let harmless = "<field name=\"name\" position=\"attributes\"/>";
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &file("name", harmless),
    )
    .await
    .unwrap();

    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session.begin_operation(["test_ext_a"]).unwrap();
    let err = load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &file("pages", "<field name=\"pages\" position=\"replace\"/>"),
    )
    .await
    .unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(err) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view book.s (test_ext_a.view_book_s) in \
         test_ext_a/data/views.xml:\nElement '<field name=\"pages\">' cannot be located in \
         parent view"
    );
    // Context attribution rule (BUG-071, R-5): when a batch of views is checked together,
    // error context is attributed to the failing view itself if it was written in the batch;
    // otherwise to the first written view of the check (lowest ID in the batch).
    // Here, failing view book.s is one of the written views in the window.
    assert_eq!(context_of(err, "name"), Ok(Value::str("book.s")));
    assert_eq!(
        context_of(err, "xmlid"),
        Ok(Value::str("test_ext_a.view_book_s"))
    );
    assert_eq!(
        context_of(err, "file"),
        Ok(Value::str("test_ext_a/data/views.xml"))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_data_load_of_arch_refreshes_arch_fs_and_arch_updated() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let load = async |tx: &mut Transaction, path: &str, mode: ConvertMode, xml: &str| {
        let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
        let file = FileContext::new("test_ext_a", path, mode);
        let mut ops = XmlDataReader::new(xml, path, "test_ext_a", false);
        session.load_file(tx, &file, &mut ops).await
    };
    load(
        &mut tx,
        "data/views.xml",
        ConvertMode::Init,
        &book_form(&["name"]),
    )
    .await
    .unwrap();
    let id = xid_id(&mut tx, "test_ext_a.view_book_form").await;
    let updated = async |tx: &mut Transaction| -> Cell {
        let views = tx.env().records(VIEW).unwrap().browse([id]).unwrap();
        views.cells(tx, "arch_updated").await.unwrap()[0].clone()
    };
    assert_eq!(updated(&mut tx).await, Cell::Bool(false));
    // a user's edit
    write(
        &mut tx,
        &[id],
        &[(
            "arch",
            s("<form><field name=\"name\"/><field name=\"pages\"/></form>"),
        )],
    )
    .await
    .unwrap();
    assert_eq!(updated(&mut tx).await, Cell::Bool(true));
    // the upgrade loads the view from another file
    load(
        &mut tx,
        "views/book.xml",
        ConvertMode::Update,
        &book_form(&["name"]),
    )
    .await
    .unwrap();
    assert_eq!(updated(&mut tx).await, Cell::Bool(false));
    assert_eq!(
        text_field(&mut tx, id, "arch_fs").await.unwrap(),
        "test_ext_a/views/book.xml"
    );
    // `arch_db` written directly changes neither
    write(
        &mut tx,
        &[id],
        &[("arch_db", s("<form><field name=\"pages\"/></form>"))],
    )
    .await
    .unwrap();
    assert_eq!(updated(&mut tx).await, Cell::Bool(false));
    assert_eq!(
        text_field(&mut tx, id, "arch_fs").await.unwrap(),
        "test_ext_a/views/book.xml"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Two views of `test_ext.book` in one data file: the root form and an extension of it whose
/// field after `name` is `field`.
fn book_root_and_extension(field: &str) -> String {
    format!(
        r#"<odoo><record id="view_book_form" model="ir.ui.view">
  <field name="name">book.form</field>
  <field name="model">test_ext.book</field>
  <field name="arch" type="xml"><form><field name="name"/><field name="pages"/></form></field>
</record><record id="view_book_ext" model="ir.ui.view">
  <field name="name">book.ext</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch" type="xml"><field name="name" position="after"><field name="{field}"/></field></field>
</record></odoo>"#
    )
}

/// Counted with `query_count` (statements, transaction control excluded) around:
/// a create of three extensions in one call, a write of `arch`, a write of `inherit_id` to the
/// parent it already has, a write of `inherit_id` to another root, and the upgrade of a data file
/// of two views where one arch changed (`ConvertMode::Update`, the whole file: the loader's own
/// lookups are in the count).
#[tokio::test]
async fn view_write_statement_counts() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_root_and_extension("pages"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let root = xid_id(&mut tx, "test_ext_a.view_book_form").await;
    let other_root = create(
        &mut tx,
        &[
            ("model", s("test_ext.book")),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ],
    )
    .await
    .unwrap();
    let ext = |anchor: &str| {
        vals(&[
            ("model", s("test_ext.book")),
            ("inherit_id", Cell::Id(root)),
            (
                "arch",
                s(&format!(
                    "<field name=\"{anchor}\" position=\"after\"><field name=\"name\"/></field>"
                )),
            ),
        ])
    };
    let mut counts = Vec::new();
    let views = tx.env().records(VIEW).unwrap();
    let before = tx.query_count();
    let created = views
        .create(&mut tx, vec![ext("name"), ext("pages"), ext("name")])
        .await
        .unwrap()
        .ids();
    counts.push(("create 3 extensions", tx.query_count() - before));

    let before = tx.query_count();
    write(
        &mut tx,
        &[created[0]],
        &[(
            "arch",
            s("<field name=\"pages\" position=\"before\"><field name=\"name\"/></field>"),
        )],
    )
    .await
    .unwrap();
    counts.push(("write arch", tx.query_count() - before));

    let before = tx.query_count();
    write(&mut tx, &[created[1]], &[("inherit_id", Cell::Id(root))])
        .await
        .unwrap();
    counts.push(("write inherit_id unchanged", tx.query_count() - before));

    let before = tx.query_count();
    write(
        &mut tx,
        &[created[2]],
        &[("inherit_id", Cell::Id(other_root))],
    )
    .await
    .unwrap();
    counts.push(("write inherit_id moved", tx.query_count() - before));
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let before = tx.query_count();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &book_root_and_extension("isbn"),
    )
    .await
    .unwrap();
    counts.push(("upgrade 2 views, 1 changed", tx.query_count() - before));
    let ext_id = xid_id(&mut tx, "test_ext_a.view_book_ext").await;
    assert!(
        text_field(&mut tx, ext_id, "arch_db")
            .await
            .unwrap()
            .contains("isbn")
    );
    tx.rollback().await.unwrap();

    assert_eq!(
        counts,
        [
            ("create 3 extensions", 3),
            ("write arch", 2),
            ("write inherit_id unchanged", 2),
            ("write inherit_id moved", 3),
            ("upgrade 2 views, 1 changed", 9),
        ]
    );
    h.cleanup().await.unwrap();
}

/// In a data load window updating multiple views, the combined-arch check runs once per load window
/// over the per-position load state instead of once per written view (
/// review D-1).
///
/// This test distinguishes batch-level whole-window validation from old per-view validation (R-2 / FD-1):
/// `ext_1` and `ext_2` are chained extensions of `view_book_form` where `ext_2` targets the field `pages`
/// inserted by `ext_1`. In an upgrade where both views' archs change (swapping their intermediate anchor
/// to `isbn`), old per-view validation fails immediately upon writing `ext_1`.
///
/// Mechanics of the old per-view failure (FD-1 / R-2):
/// When `ext_1` is written, `validation_targets` returns `targets.validated = [ext_1_id]` and empty
/// `targets.combined` (as the root `view_book_form` is already covered by `ext_1`). Because `loaded_only`
/// (`validation.rs:723`) only filters `targets.combined` (primary views), it does not filter extension
/// children. When `catalogue.combine_many` evaluates `ext_1`, `ViewCatalogue` walks up to `view_book_form`
/// and applies all active extension children via `applied_children`. `ViewCatalogue` operates on the
/// descriptors fetched from the database and has no access to loader `LoadState` or `BATCH_WRITTEN`.
/// Therefore, `ext_2` (still holding its pre-update arch in PostgreSQL targeting `pages`) is combined.
/// Because `ext_1` has already been updated to insert `isbn` instead of `pages`, `ext_2` cannot locate
/// `pages` and fails with `ErrorKind::Validation` ("Element '<field name=\"pages\">' cannot be located in parent view").
///
/// Under batch-level upgrade check, validation during `IrUiView::write` is
/// deferred while `in_tracked_batch` is true. Both `ext_1` and `ext_2` are written to PostgreSQL before
/// `_check_xml` runs. When `_check_xml` runs at the window end, `ext_1` has inserted `isbn` and `ext_2`
/// targets `isbn` to insert `pages`—both new archs are present, and validation succeeds cleanly (GREEN).
#[tokio::test]
async fn batch_level_upgrade_check_in_load_window() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name"]),
    )
    .await
    .unwrap();

    let esc = |xml: &str| {
        xml.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let two_exts = |val1: &str, target2: &str, val2: &str| {
        let arch1 = esc(&format!(
            "<field name=\"name\" position=\"after\"><field name=\"{val1}\"/></field>"
        ));
        let arch2 = esc(&format!(
            "<field name=\"{target2}\" position=\"after\"><field name=\"{val2}\"/></field>"
        ));
        format!(
            r#"<odoo><record id="view_book_ext_1" model="ir.ui.view">
  <field name="name">book.ext.1</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{arch1}</field>
</record><record id="view_book_ext_2" model="ir.ui.view">
  <field name="name">book.ext.2</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{arch2}</field>
</record></odoo>"#
        )
    };
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &two_exts("pages", "pages", "isbn"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 1. Upgrade both extension views in one window: both archs change.
    // On per-view code, writing ext_1 immediately triggers validation which fails because
    // ext_2 in DB still targets 'pages'. On batch-level code, both writes complete before
    // _check_xml validates the whole window.
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &two_exts("isbn", "isbn", "pages"),
    )
    .await
    .unwrap();

    // Verify both archs were updated
    let ext1_id = xid_id(&mut tx, "test_ext_a.view_book_ext_1").await;
    let ext2_id = xid_id(&mut tx, "test_ext_a.view_book_ext_2").await;
    assert!(
        text_field(&mut tx, ext1_id, "arch_db")
            .await
            .unwrap()
            .contains("isbn")
    );
    assert!(
        text_field(&mut tx, ext2_id, "arch_db")
            .await
            .unwrap()
            .contains("pages")
    );

    // 2. A broken view in the upgrade batch fails the batch-level check (R-1):
    // Full error message and context (name, xmlid, file) are verified.
    let bad_ext = two_exts("isbn", "nonexistent_anchor", "pages");
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let err = load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &bad_ext,
    )
    .await
    .unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(err) = err.kind() else {
        panic!("{err:?}")
    };
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Error while validating view book.ext.2 (test_ext_a.view_book_ext_2) in \
         test_ext_a/data/views.xml:\nElement '<field name=\"nonexistent_anchor\">' cannot be \
         located in parent view"
    );
    assert_eq!(context_of(err, "name"), Ok(Value::str("book.ext.2")));
    assert_eq!(
        context_of(err, "xmlid"),
        Ok(Value::str("test_ext_a.view_book_ext_2"))
    );
    assert_eq!(
        context_of(err, "file"),
        Ok(Value::str("test_ext_a/data/views.xml"))
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Upgrading a 2-view window with 2 changed archs costs exactly 8 SQL statements:
/// external id lookup, target lookup, parent read x2, groups read x2, UPDATE, batch-level _check_xml x1.
/// Retained as an explicit milestone-only performance pin (FD-2); semantic coverage in
/// `batch_level_upgrade_check_in_load_window`.
#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
async fn batch_level_upgrade_check_statement_cost() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &book_form(&["name"]),
    )
    .await
    .unwrap();

    let esc = |xml: &str| {
        xml.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let two_exts = |val1: &str, target2: &str, val2: &str| {
        let arch1 = esc(&format!(
            "<field name=\"name\" position=\"after\"><field name=\"{val1}\"/></field>"
        ));
        let arch2 = esc(&format!(
            "<field name=\"{target2}\" position=\"after\"><field name=\"{val2}\"/></field>"
        ));
        format!(
            r#"<odoo><record id="view_book_ext_1" model="ir.ui.view">
  <field name="name">book.ext.1</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{arch1}</field>
</record><record id="view_book_ext_2" model="ir.ui.view">
  <field name="name">book.ext.2</field>
  <field name="model">test_ext.book</field>
  <field name="inherit_id" ref="view_book_form"/>
  <field name="arch">{arch2}</field>
</record></odoo>"#
        )
    };
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Init,
        &two_exts("pages", "pages", "isbn"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    let before = tx.query_count();
    load_views(
        &mut session,
        &mut tx,
        "test_ext_a",
        ConvertMode::Update,
        &two_exts("isbn", "isbn", "pages"),
    )
    .await
    .unwrap();
    let upgrade_count = tx.query_count() - before;

    // Statement count for upgrading a 2-view window with 2 changed archs:
    // exactly 8 statements (external id lookup, target lookup, parent read x2, groups read x2,
    // UPDATE, batch-level _check_xml x1).
    assert_eq!(upgrade_count, 8, "upgrade statement count: {upgrade_count}");

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Two users move an extension each into the other's tree at the same time (review D-3): every
/// root lock of a write of `inherit_id` is taken in one statement in id order before the write,
/// so the second writer waits for the first and fails with a serialization error (`40001`, the
/// retry the ORM already does) instead of both deadlocking (`40P01`) on roots locked in opposite
/// orders.
#[tokio::test]
async fn crossed_moves_between_two_trees_serialize_without_a_deadlock() {
    let h = book_harness().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let form = || {
        vec![
            ("model", s("test_ext.book")),
            ("arch", s("<form><field name=\"name\"/></form>")),
        ]
    };
    let x = create(&mut tx, &form()).await.unwrap();
    let y = create(&mut tx, &form()).await.unwrap();
    let ext = |parent: i64| {
        vec![
            ("model", s("test_ext.book")),
            ("inherit_id", Cell::Id(parent)),
            (
                "arch",
                s("<field name=\"name\" position=\"after\"><field name=\"pages\"/></field>"),
            ),
        ]
    };
    let a = create(&mut tx, &ext(x)).await.unwrap();
    let b = create(&mut tx, &ext(y)).await.unwrap();
    tx.commit().await.unwrap();

    let mut one = h.begin().await.unwrap();
    let mut two = h.begin().await.unwrap();
    one.query("SELECT 1", &[]).await.unwrap();
    two.query("SELECT 1", &[]).await.unwrap();
    let (first, second) = tokio::join!(
        async {
            write(&mut one, &[a], &[("inherit_id", Cell::Id(y))]).await?;
            one.commit().await
        },
        async {
            write(&mut two, &[b], &[("inherit_id", Cell::Id(x))]).await?;
            two.commit().await
        }
    );
    let failed = match (first, second) {
        (Ok(()), Err(err)) | (Err(err), Ok(())) => err,
        other => panic!("one move must commit and the other fail: {other:?}"),
    };
    assert_eq!(failed.kind(), ErrorKind::Concurrency, "{failed:?}");
    assert_eq!(failed.sqlstate(), Some("40001"), "{failed:?}");
    h.cleanup().await.unwrap();
}

/// A `force_delete` of a set holding a view and a view built on it, further apart than one
/// delete chunk (`DELETE_CHUNK`, 10,000 ids), deletes children first, so no chunk removes a
/// parent while its child, in a later chunk, still names it under `ondelete="restrict"` (review
/// D-6: the uninstall's copy purge passes such sets). The views are written with SQL: there are
/// more than the chunk.
#[tokio::test]
async fn a_forced_delete_across_chunks_deletes_children_first() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        &[
            ("key", s("m1.parent")),
            ("type", s("qweb")),
            ("arch", s("<t><p/></t>")),
        ],
    )
    .await
    .unwrap();
    let chunk: i32 = 10_000;
    let fillers: Vec<i64> = tx
        .query(
            "INSERT INTO ir_ui_view (name, key, type, mode, priority, active, arch_db) \
             SELECT 'filler' || n, 'm1.filler' || n, 'qweb', 'primary', 16, true, '<t/>' \
               FROM generate_series(1, $1) AS n RETURNING id",
            &[&chunk],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    let child: i64 = tx
        .query(
            "INSERT INTO ir_ui_view (name, key, type, mode, priority, active, arch_db, inherit_id) \
             VALUES ('child', 'm1.child', 'qweb', 'extension', 16, true, \
                     '<p position=\"inside\"/>', $1) RETURNING id",
            &[&parent],
        )
        .await
        .unwrap()[0]
        .get(0);
    // the parent first, the child more than a chunk later
    let mut ids = vec![parent];
    ids.extend(&fillers);
    ids.push(child);
    tx.env()
        .records(VIEW)
        .unwrap()
        .with_context([("force_delete", Value::Bool(true))])
        .browse(ids.iter().copied())
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    let left: i64 = tx
        .query(
            "SELECT count(*) FROM ir_ui_view WHERE id = ANY($1)",
            &[&ids],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn leaving_an_extension_scope_revalidates_the_old_tree() {
    check_former_scope(false).await;
}

#[tokio::test]
async fn becoming_primary_revalidates_the_old_extension_scope() {
    check_former_scope(true).await;
}

async fn check_former_scope(become_primary: bool) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut roots = Vec::new();
    for name in ["old root", "new root"] {
        roots.push(
            create(
                &mut tx,
                &[
                    ("name", s(name)),
                    ("type", s("qweb")),
                    ("arch_db", s("<div/>")),
                ],
            )
            .await
            .unwrap(),
        );
    }
    let extension = create(
        &mut tx,
        &[
            ("name", s("contribution")),
            ("inherit_id", Cell::Id(roots[0])),
            ("arch_db", s("<div position=\"inside\"><anchor/></div>")),
        ],
    )
    .await
    .unwrap();
    let primary = create(
        &mut tx,
        &[
            ("name", s("dependent primary")),
            ("mode", s("primary")),
            ("inherit_id", Cell::Id(roots[0])),
            (
                "arch_db",
                s("<anchor position=\"replace\"><replacement/></anchor>"),
            ),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    {
        let changes = if become_primary {
            vec![("mode", s("primary"))]
        } else {
            vec![("inherit_id", Cell::Id(roots[1]))]
        };
        let error = try_write(&mut tx, &[extension], &changes).await;
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "Error while validating view dependent primary:\nElement '<anchor>' cannot be located in parent view"
        );
        let view = tx.env().records(VIEW).unwrap().browse([extension]).unwrap();
        assert_eq!(
            view.cells(&mut tx, "inherit_id").await.unwrap(),
            vec![Cell::Id(roots[0])]
        );
        assert_eq!(mode(&mut tx, extension).await, "extension");
        assert_eq!(
            combined(&mut tx, &[roots[0], extension, primary], primary).await,
            "<div><replacement/></div>"
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Regression CDB-2: an ORM edit to a committed external id is visible to selection immediately.
#[tokio::test]
async fn view_ref_selection_flushes_dirty_external_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut views = Vec::new();
    for name in ["first", "second"] {
        views.push(
            create(
                &mut tx,
                &[
                    ("name", s(name)),
                    ("model", s(VIEW)),
                    ("arch", s("<form><field name=\"name\"/></form>")),
                ],
            )
            .await
            .unwrap(),
        );
    }
    bind(&mut tx, VIEW, "base.dirty_view_ref", views[0]).await;
    let row = tx
        .query(
            "SELECT id FROM ir_model_data WHERE module = $1 AND name = $2",
            &[&"base", &"dirty_view_ref"],
        )
        .await
        .unwrap();
    let mapping: i64 = row[0].get(0);
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("ir.model.data")
        .unwrap()
        .browse([mapping])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("res_id", Cell::Id(views[1])),
        )
        .await
        .unwrap();
    let form = ViewType::new(&tx, "form").unwrap();
    let context = Context::from_pairs([("form_view_ref", Value::str("base.dirty_view_ref"))]);
    let selected = select_view(&mut tx, &ViewRequest::new(VIEW, &form).context(&context))
        .await
        .unwrap();
    assert_eq!(selected.id(), ViewId::new(views[1]).unwrap());
    assert!(selected.notices().is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// DD-1: a cold parent read must not prefetch the view architecture.
#[tokio::test]
async fn cold_parent_changes_fetch_only_the_parent_column() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut roots = Vec::new();
    for name in ["old", "new"] {
        roots.push(
            create(
                &mut tx,
                &[
                    ("name", s(name)),
                    ("type", s("qweb")),
                    ("arch_db", s("<div/>")),
                ],
            )
            .await
            .unwrap(),
        );
    }
    let child = create(
        &mut tx,
        &[
            ("inherit_id", Cell::Id(roots[0])),
            ("arch_db", s("<div position=\"inside\"><child/></div>")),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for (label, changes) in [
        ("unchanged", vec![("inherit_id", Cell::Id(roots[0]))]),
        ("moved", vec![("inherit_id", Cell::Id(roots[1]))]),
        (
            "arch",
            vec![("arch_db", s("<div position=\"inside\"><updated/></div>"))],
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        tx.capture_statements(true);
        let before = tx.query_count();
        write(&mut tx, &[child], &changes).await.unwrap();
        let count = tx.query_count() - before;
        println!("COLD_VIEW_WRITE_{label}={count}");
        println!("COLD_VIEW_STATEMENTS_{label}={:?}", tx.captured());
        if label != "arch" {
            let parent_read = tx.captured().first().expect("cold parent read");
            assert!(parent_read.contains("inherit_id"), "{parent_read}");
            assert!(
                !parent_read.contains("arch_db"),
                "parent read must not prefetch arch: {parent_read}"
            );
            assert_eq!(count, if label == "unchanged" { 4 } else { 5 });
        } else {
            assert_eq!(count, 2);
        }
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}

/// `test_groups_field`, `test_groups_field_removed` and `test_attrs_groups_behavior`
/// :
/// - A form with `<field name="a" groups="base.group_system"/>`, `<field name="b" groups="!base.group_system"/>`,
///   a field whose definition has `groups="base.group_system"`, and `<t groups="base.group_user"><field name="c"/></t>`.
/// - Admin sees `a`, the restricted field, `c` (unwrapped from `<t>`) and not `b`.
/// - Internal non-admin user sees `b` and `c` only.
/// - The cached post-processed arch is shared across users and filtered per-user after the cache.
/// - `groups` attributes are stripped from the arch.
/// - `test_groups_field_removed`: `<span>(<field name="name" groups="base.group_portal"/>)</span>` -> `<span>()</span>` (tail text preserved).
/// - `test_attrs_groups_behavior`: arbitrary tags (`<div>`) and fields with groups.
/// - Unknown groups (`base.unknown`, `!base.unknown`) evaluate without error (positive -> false, negative -> true).
/// - `base.group_no_one` counts only in debug mode (`env.with_debug(true)`), and only for its members
///   (the guard is membership and debug, as the compiled policy decides it).
/// - Calling `get_view` without read access on the model returns an `Access` error.
/// - Warm `get_view` costs 0 statements beyond opening flight and principal resolution.
#[tokio::test]
async fn spec_data_060_groups_in_arch_filter_elements_per_user() {
    let admin_uid = 10;
    let demo_uid = 11;
    let foreign_uid = 12;

    let policy = FieldPolicy::new()
        .member(admin_uid, ["base.group_system", "base.group_user"])
        .member(demo_uid, ["base.group_user", "base.group_no_one"])
        .deny_model("test_view_groups.denied");

    let h = harness_with_policy(policy).await;
    let mut tx = h.begin().await.unwrap();

    let model_name = "test_view_groups.model";
    let form_type = ViewType::new(&tx, "form").unwrap();

    // 1. Core case:
    let core_arch = r#"<form>
        <field name="a" groups="base.group_system"/>
        <field name="b" groups="!base.group_system"/>
        <field name="restricted"/>
        <t groups="base.group_user"><field name="c"/></t>
    </form>"#;

    let view_core = create(
        &mut tx,
        &[
            ("name", s("core view")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(10)),
            ("arch", s(core_arch)),
        ],
    )
    .await
    .unwrap();

    let req = ViewRequest::new(model_name, &form_type).view_id(ViewId::new(view_core).unwrap());

    // Call as admin:
    let admin_env = tx.env().with_user(admin_uid);
    let resp_admin = get_view(&mut tx, &admin_env, &req).await.unwrap();
    let arch_admin = resp_admin.arch();

    assert!(
        arch_admin.contains(r#"<field name="a"/>"#),
        "admin sees a: {arch_admin}"
    );
    assert!(
        arch_admin.contains(r#"<field name="restricted"/>"#),
        "admin sees restricted: {arch_admin}"
    );
    assert!(
        arch_admin.contains(r#"<field name="c"/>"#),
        "admin sees c: {arch_admin}"
    );
    assert!(
        !arch_admin.contains(r#"name="b""#),
        "admin must NOT see b: {arch_admin}"
    );
    assert!(
        !arch_admin.contains("<t>"),
        "t must be unwrapped: {arch_admin}"
    );
    assert!(
        !arch_admin.contains("</t>"),
        "t must be unwrapped: {arch_admin}"
    );
    assert!(
        !arch_admin.contains("groups="),
        "groups attribute must be stripped: {arch_admin}"
    );

    // Call as demo (internal non-admin):
    let demo_env = tx.env().with_user(demo_uid);
    let resp_demo = get_view(&mut tx, &demo_env, &req).await.unwrap();
    let arch_demo = resp_demo.arch();

    // the compiled bundle is shared by both users: one miss, the second call is a hit
    // (T-01)
    assert_eq!(
        tx.ormcache_stats("ir.ui.view", "get_view").1,
        1,
        "one compiled bundle for both users"
    );

    assert!(
        arch_demo.contains(r#"<field name="b"/>"#),
        "demo sees b: {arch_demo}"
    );
    assert!(
        arch_demo.contains(r#"<field name="c"/>"#),
        "demo sees c: {arch_demo}"
    );
    assert!(
        !arch_demo.contains(r#"name="a""#),
        "demo must NOT see a: {arch_demo}"
    );
    assert!(
        !arch_demo.contains(r#"name="restricted""#),
        "demo must NOT see restricted: {arch_demo}"
    );
    assert!(
        !arch_demo.contains("<t>"),
        "t must be unwrapped: {arch_demo}"
    );
    assert!(
        !arch_demo.contains("</t>"),
        "t must be unwrapped: {arch_demo}"
    );
    assert!(
        !arch_demo.contains("groups="),
        "groups attribute must be stripped: {arch_demo}"
    );

    // 2. Warm get_view: hits cache, costs 1 statement for select_view (0 tree fetch statements)
    tx.capture_statements(true);
    let before_queries = tx.query_count();
    let warm_resp = get_view(&mut tx, &demo_env, &req).await.unwrap();
    let query_diff = tx.query_count() - before_queries;
    assert_eq!(warm_resp.arch(), arch_demo);
    assert_eq!(
        query_diff,
        1,
        "warm get_view must cost 1 statement (select_view): {:?}",
        tx.captured()
    );

    // 3. test_groups_field_removed: tail text preservation
    let tail_arch = r#"<form>
        <span class="oe_inline">(<field name="name" groups="base.group_portal"/>)</span>
    </form>"#;
    let view_tail = create(
        &mut tx,
        &[
            ("name", s("tail view")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(20)),
            ("arch", s(tail_arch)),
        ],
    )
    .await
    .unwrap();
    let req_tail =
        ViewRequest::new(model_name, &form_type).view_id(ViewId::new(view_tail).unwrap());
    let resp_tail = get_view(&mut tx, &demo_env, &req_tail).await.unwrap();
    assert!(
        resp_tail
            .arch()
            .contains(r#"<span class="oe_inline">()</span>"#),
        "parentheses must be preserved when field is stripped: {}",
        resp_tail.arch()
    );

    // 4. test_attrs_groups_behavior:
    let attrs_arch = r#"<form>
        <field name="name"/>
        <field name="company_id" groups="base.group_system"/>
        <div id="foo"/>
        <div id="bar" groups="base.group_system"/>
    </form>"#;
    let view_attrs = create(
        &mut tx,
        &[
            ("name", s("attrs view")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(30)),
            ("arch", s(attrs_arch)),
        ],
    )
    .await
    .unwrap();
    let req_attrs =
        ViewRequest::new(model_name, &form_type).view_id(ViewId::new(view_attrs).unwrap());

    // demo sees name and foo, not company_id and not bar
    let demo_attrs = get_view(&mut tx, &demo_env, &req_attrs).await.unwrap();
    assert!(demo_attrs.arch().contains(r#"<field name="name"/>"#));
    assert!(demo_attrs.arch().contains(r#"<div id="foo"/>"#));
    assert!(!demo_attrs.arch().contains("company_id"));
    assert!(!demo_attrs.arch().contains(r#"id="bar""#));

    // admin sees all four
    let admin_attrs = get_view(&mut tx, &admin_env, &req_attrs).await.unwrap();
    assert!(admin_attrs.arch().contains(r#"<field name="name"/>"#));
    assert!(admin_attrs.arch().contains(r#"<field name="company_id"/>"#));
    assert!(admin_attrs.arch().contains(r#"<div id="foo"/>"#));
    assert!(admin_attrs.arch().contains(r#"<div id="bar"/>"#));

    // 5. Unknown groups:
    let unk_arch = r#"<form>
        <field name="a" groups="base.unknown_group"/>
        <field name="b" groups="!base.unknown_group"/>
    </form>"#;
    let view_unk = create(
        &mut tx,
        &[
            ("name", s("unknown view")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(40)),
            ("arch", s(unk_arch)),
        ],
    )
    .await
    .unwrap();
    let req_unk = ViewRequest::new(model_name, &form_type).view_id(ViewId::new(view_unk).unwrap());
    let resp_unk = get_view(&mut tx, &demo_env, &req_unk).await.unwrap();
    assert!(
        !resp_unk.arch().contains(r#"name="a""#),
        "unknown group positive must deny"
    );
    assert!(
        resp_unk.arch().contains(r#"<field name="b"/>"#),
        "!unknown group must allow"
    );

    // 6. base.group_no_one (debug mode):
    let no_one_arch = r#"<form>
        <field name="a" groups="base.group_no_one"/>
        <field name="b"/>
    </form>"#;
    let view_no_one = create(
        &mut tx,
        &[
            ("name", s("no_one view")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(50)),
            ("arch", s(no_one_arch)),
        ],
    )
    .await
    .unwrap();
    let req_no_one =
        ViewRequest::new(model_name, &form_type).view_id(ViewId::new(view_no_one).unwrap());

    // Without debug mode:
    let resp_standard = get_view(&mut tx, &demo_env.with_debug(false), &req_no_one)
        .await
        .unwrap();
    assert!(
        !resp_standard.arch().contains(r#"name="a""#),
        "group_no_one hidden without debug"
    );
    assert!(resp_standard.arch().contains(r#"<field name="b"/>"#));

    // With debug mode:
    let resp_debug = get_view(&mut tx, &demo_env.with_debug(true), &req_no_one)
        .await
        .unwrap();
    assert!(
        resp_debug.arch().contains(r#"<field name="a"/>"#),
        "group_no_one visible in debug"
    );
    assert!(resp_debug.arch().contains(r#"<field name="b"/>"#));

    // 7. Access refusal when caller lacks read access on model:
    let denied_req = ViewRequest::new("test_view_groups.denied", &form_type);
    let foreign_env = tx.env().with_user(foreign_uid);
    let access_err = get_view(&mut tx, &foreign_env, &denied_req)
        .await
        .unwrap_err();
    assert_eq!(access_err.kind(), ErrorKind::Access);
    assert!(access_err.to_string().contains("do not let you read"));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `testDefaultViewBase`, `testDefaultViewPrimary` and `test_view_ref` through `get_view`:
/// - `get_view` without a view id chooses the priority-1 primary view (primary inheriting views count).
/// - A `form_view_ref` context with a fully qualified XML ID overrides.
/// - An unqualified, missing, other-model or other-type ref falls back to the default view with a notice.
#[tokio::test]
async fn spec_data_043_get_view_selection_and_view_ref() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let model_name = "test_view_groups.model";
    let form_type = ViewType::new(&tx, "form").unwrap();

    let view1 = create(
        &mut tx,
        &[
            ("name", s("p10")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(10)),
            ("arch", s("<form><field name=\"a\"/></form>")),
        ],
    )
    .await
    .unwrap();

    let _view2 = create(
        &mut tx,
        &[
            ("name", s("p5")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(5)),
            ("arch", s("<form><field name=\"b\"/></form>")),
        ],
    )
    .await
    .unwrap();

    // Primary inheriting view on view1 with priority 1:
    let view3 = create(
        &mut tx,
        &[
            ("name", s("p1 on p10")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("inherit_id", Cell::Id(view1)),
            ("mode", s("primary")),
            ("priority", Cell::Int(1)),
            (
                "arch",
                s("<form position=\"inside\"><field name=\"c\"/></form>"),
            ),
        ],
    )
    .await
    .unwrap();

    bind(&mut tx, VIEW, "base.test_v1_ref", view1).await;

    // 1. get_view without view_id selects priority-1 primary view (view3)
    let env = tx.env();
    let req_default = ViewRequest::new(model_name, &form_type);
    let resp = get_view(&mut tx, &env, &req_default).await.unwrap();
    assert_eq!(resp.view_id(), ViewId::new(view3).unwrap());
    assert!(resp.notices().is_empty());
    assert!(resp.arch().contains(r#"<field name="a"/>"#));
    assert!(resp.arch().contains(r#"<field name="c"/>"#));

    // 2. form_view_ref context overrides
    let ctx_override = Context::from_pairs([("form_view_ref", Value::str("base.test_v1_ref"))]);
    let req_override = ViewRequest::new(model_name, &form_type).context(&ctx_override);
    let resp_override = get_view(&mut tx, &env, &req_override).await.unwrap();
    assert_eq!(resp_override.view_id(), ViewId::new(view1).unwrap());
    assert!(resp_override.notices().is_empty());

    // 3. Unqualified ref falls back to default view with notice
    let ctx_unqualified = Context::from_pairs([("form_view_ref", Value::str("unqualified_ref"))]);
    let req_unqualified = ViewRequest::new(model_name, &form_type).context(&ctx_unqualified);
    let resp_unqualified = get_view(&mut tx, &env, &req_unqualified).await.unwrap();
    assert_eq!(resp_unqualified.view_id(), ViewId::new(view3).unwrap());
    assert_eq!(resp_unqualified.notices().len(), 1);
    match &resp_unqualified.notices()[0] {
        ViewNotice::RefNotQualified {
            key,
            value,
            problem,
        } => {
            assert_eq!(key, "form_view_ref");
            assert_eq!(value, "unqualified_ref");
            assert!(problem.contains("has no module part"));
        }
        other => panic!("expected RefNotQualified, got {other:?}"),
    }

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the checked filter refuses it)
///
/// The lowest-priority primary view of the model is limited to a group the user is not in, a
/// group-less primary follows.
/// Asking for the group-less view by id serves it; superuser mode does not change the answer.
#[tokio::test]
async fn spec_data_101_default_selection_refuses_a_group_limited_primary() {
    let nobody = 12;
    let h = harness_with_policy(FieldPolicy::new()).await;
    let mut tx = h.begin().await.unwrap();
    let model_name = "test_view_groups.model";
    let form_type = ViewType::new(&tx, "form").unwrap();

    let open = create(
        &mut tx,
        &[
            ("name", s("open")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(5)),
            ("arch", s("<form><field name=\"a\"/></form>")),
        ],
    )
    .await
    .unwrap();
    let limited = create(
        &mut tx,
        &[
            ("name", s("limited")),
            ("model", s(model_name)),
            ("type", s("form")),
            ("priority", Cell::Int(1)),
            ("arch", s("<form><field name=\"b\"/></form>")),
        ],
    )
    .await
    .unwrap();
    // the harness has none of base's data: groups are created as during the base installation
    erp_testkit::base_install::set_base_installing(&mut tx, true).unwrap();
    let group = tx
        .env()
        .records("res.groups")
        .unwrap()
        .create(&mut tx, vec![vals(&[("name", s("G"))])])
        .await
        .unwrap()
        .ids()[0];
    let views = tx.env().records(VIEW).unwrap().browse([limited]).unwrap();
    views
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "group_ids",
                [Command::set([RecId::from_real(group).unwrap()])],
            ),
        )
        .await
        .unwrap();

    // selection does not skip the limited view
    assert_eq!(
        default_view(&mut tx, Some(model_name), &form_type)
            .await
            .unwrap(),
        Some(ViewId::new(limited).unwrap())
    );

    let env = tx.env().with_user(nobody);
    let req = ViewRequest::new(model_name, &form_type);
    let error = get_view(&mut tx, &env, &req).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.to_string(),
        format!("view {limited} is restricted to groups the user is not in")
    );

    // the group-less view, asked for by id, is served
    let by_id = ViewRequest::new(model_name, &form_type).view_id(ViewId::new(open).unwrap());
    let served = get_view(&mut tx, &env, &by_id).await.unwrap();
    assert_eq!(served.view_id(), ViewId::new(open).unwrap());

    // superuser mode does not change guard answers: the limited view is refused there as well
    let sudo = tx.env().with_user(nobody).sudo(true);
    let error = get_view(&mut tx, &sudo, &req).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.to_string(),
        format!("view {limited} is restricted to groups the user is not in")
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
