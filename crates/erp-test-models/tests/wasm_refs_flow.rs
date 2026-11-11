//! Typed model references and guest `create`: the fixture
//! addon `test_wasm_refs` names fields of `test.wasm.dual` that two other addons declare
//! (`model_ref!`), creates and writes records with `Vals`, follows a many2one with `related` and
//! overrides `create` of the model. One source runs natively (tests below, always) and as a
//! `wasm32-wasip2` component (the `#[ignore]`d tests, which fail, never skip, without the
//! artefact: `scripts/ci.sh` step `wasm-dual-describe` builds it and sets
//! `ERP_WASM_REFS_COMPONENT`). Both must give the same answers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_test_wasm_dual_a::ADDON as ADDON_A;
use erp_addon_test_wasm_dual_b::ADDON as ADDON_B;
use erp_addon_test_wasm_refs::{ADDON as ADDON_REFS, CREATE_MARK};
use erp_fields::ErrorKind;
use erp_modules::ModuleName;
use erp_modules::manifest::Manifest;
use erp_modules::wasm_lifecycle::{check_refs_in_registry, refs_from_describe};
use erp_orm::{Context, Orm, Transaction, call_method_json};
use erp_sdk::Value;
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_wasm_host::{
    CallableMethods, Limits, OrmCapabilities, OrmOp, WasmAddon, WasmHost, callable_methods,
};

const DUAL: &str = "test.wasm.dual";
const LINE: &str = "test.wasm.refs.line";

struct World {
    h: Harness,
    orm: Orm,
}

/// Registry of the three addons; the native build of `test_wasm_refs` runs its methods.
async fn native_world() -> World {
    let h = Harness::builder(vec![&base_users::ADDON, &ADDON_A, &ADDON_B, &ADDON_REFS])
        .build()
        .await
        .unwrap();
    let orm = h.orm().clone();
    World { h, orm }
}

fn caps_all() -> OrmCapabilities {
    OrmCapabilities::NONE
        .with(OrmOp::Read)
        .with(OrmOp::Write)
        .with(OrmOp::Create)
        .with(OrmOp::Unlink)
        .with(OrmOp::CallMethod)
}

/// The same registry, with the methods of `test_wasm_refs` running as the built component.
async fn component_world(caps: OrmCapabilities) -> World {
    let path = std::env::var("ERP_WASM_REFS_COMPONENT").unwrap_or_else(|_| {
        panic!("ERP_WASM_REFS_COMPONENT must name the test_wasm_refs component")
    });
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let h = Harness::builder(vec![&base_users::ADDON, &ADDON_A, &ADDON_B, &ADDON_REFS])
        .as_user(4)
        .build()
        .await
        .unwrap();
    {
        let mut tx = h
            .orm()
            .begin(erp_orm::PrincipalHint::system())
            .await
            .unwrap();
        tx.batch(
            "INSERT INTO res_users (id, name, login, active) VALUES (4, 'four', 'four', true)",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let host = Arc::new(WasmHost::new().unwrap());
    let component = host.component(&bytes).unwrap();
    let limits = Limits::default();
    let probe = WasmAddon::new(
        Arc::clone(&host),
        ADDON_REFS.name,
        component.clone(),
        limits,
        caps,
        CallableMethods::default(),
    );
    let describe = probe.describe().await.unwrap();
    let callable = callable_methods(h.orm(), ADDON_REFS.name, &describe).unwrap();
    let addon = WasmAddon::new(host, ADDON_REFS.name, component, limits, caps, callable);
    let wasm = addon.method_impls(&describe).unwrap();
    let orm = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(wasm)
        .build()
        .unwrap();
    World { h, orm }
}

async fn begin(w: &World) -> Transaction {
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    w.orm.begin_with(w.h.principal(), ctx).await.unwrap()
}

async fn call(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    method: &str,
    args: &str,
) -> erp_fields::Result<serde_json::Value> {
    let records = tx
        .env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())?;
    let text = call_method_json(tx, &records, method, args).await?;
    Ok(serde_json::from_str(&text).unwrap())
}

async fn read(tx: &mut Transaction, model: &str, id: i64, field: &str) -> Value {
    let records = tx.env().records(model).unwrap().browse([id]).unwrap();
    records
        .read(tx, &[field])
        .await
        .unwrap()
        .remove(0)
        .remove(field)
        .unwrap()
}

/// The lending-shaped flow, identical in both backends: create two duals through typed values
/// (the `create` override marks the names), write two fields at once, set and read a field that
/// another addon declared, create a record of another model pointing at a dual, follow the
/// many2one back to a recordset, and take an empty recordset of another model.
async fn scenario(w: &World) {
    let mut tx = begin(w).await;
    let made = call(&mut tx, DUAL, &[], "spawn", r#"[["a","b"]]"#)
        .await
        .unwrap();
    let ids: Vec<i64> = serde_json::from_value(made).unwrap();
    assert_eq!(ids.len(), 2);
    // created through the chain: the override of this very addon marked both names
    for (id, name) in ids.iter().zip(["a", "b"]) {
        assert_eq!(
            read(&mut tx, DUAL, *id, "name").await,
            Value::Str(format!("{name}{CREATE_MARK}").into())
        );
        assert_eq!(read(&mut tx, DUAL, *id, "qty").await, Value::Int(7));
    }
    // several fields in one write
    call(&mut tx, DUAL, &ids[..1], "rename", r#"["z", 9]"#)
        .await
        .unwrap();
    assert_eq!(
        read(&mut tx, DUAL, ids[0], "name").await,
        Value::Str("z".into())
    );
    assert_eq!(read(&mut tx, DUAL, ids[0], "qty").await, Value::Int(9));
    assert_eq!(
        read(&mut tx, DUAL, ids[1], "qty").await,
        Value::Int(7),
        "other record untouched"
    );
    // a field declared by test_wasm_dual_b
    let noted = call(&mut tx, DUAL, &ids[..1], "annotate", r#"["hello"]"#)
        .await
        .unwrap();
    assert_eq!(noted, serde_json::json!("hello"));
    // a date and a text in one write
    let day = call(&mut tx, DUAL, &ids[..1], "stamp", "[19000]")
        .await
        .unwrap();
    assert_eq!(day, serde_json::json!(19000));
    assert_eq!(
        read(&mut tx, DUAL, ids[0], "note").await,
        Value::Str("stamped".into())
    );
    // a record of another model, then the many2one followed to a recordset
    let l1 = call(&mut tx, DUAL, &ids[..1], "add_line", r#"["one"]"#)
        .await
        .unwrap();
    let l2 = call(&mut tx, DUAL, &ids[..1], "add_line", r#"["two"]"#)
        .await
        .unwrap();
    let l3 = call(&mut tx, DUAL, &ids[1..], "add_line", r#"["three"]"#)
        .await
        .unwrap();
    let lines: Vec<i64> = [l1, l2, l3].iter().map(|v| v.as_i64().unwrap()).collect();
    let names = call(&mut tx, LINE, &lines[..1], "dual_names", "null")
        .await
        .unwrap();
    assert_eq!(names, serde_json::json!(["z"]));
    // two lines of the same dual give that dual once; three lines give the two duals in order
    let one = call(&mut tx, LINE, &lines[..2], "dual_ids", "null")
        .await
        .unwrap();
    assert_eq!(one, serde_json::json!([ids[0]]));
    let both = call(&mut tx, LINE, &lines, "dual_ids", "null")
        .await
        .unwrap();
    assert_eq!(both, serde_json::json!([ids[0], ids[1]]));
    // an empty recordset of another model
    let empty = call(&mut tx, LINE, &lines[..1], "empty_duals", "null")
        .await
        .unwrap();
    assert_eq!(empty, serde_json::json!(0));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_native_backend_runs_the_flow() {
    scenario(&native_world().await).await;
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_refs; run by scripts/ci.sh"]
async fn the_component_backend_runs_the_same_flow() {
    scenario(&component_world(caps_all()).await).await;
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_refs; run by scripts/ci.sh"]
async fn a_create_without_the_capability_is_refused() {
    let caps = OrmCapabilities::NONE
        .with(OrmOp::Read)
        .with(OrmOp::Write)
        .with(OrmOp::CallMethod);
    let w = component_world(caps).await;
    let mut tx = begin(&w).await;
    let err = call(&mut tx, DUAL, &[], "spawn", r#"[["a"]]"#)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(err.message().contains("orm:create"), "{}", err.message());
    assert!(!tx.is_poisoned(), "an ordinary refusal");
    let dual = tx.env().records(DUAL).unwrap();
    assert_eq!(
        dual.search(&mut tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap()
            .len(),
        0
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_refs; run by scripts/ci.sh"]
async fn a_multi_field_write_without_the_capability_is_refused() {
    // everything but `orm:write`: the `create` layer of the component may still continue the chain
    let caps = OrmCapabilities::NONE
        .with(OrmOp::Read)
        .with(OrmOp::Create)
        .with(OrmOp::Unlink)
        .with(OrmOp::CallMethod);
    let w = component_world(caps).await;
    let mut tx = begin(&w).await;
    let id = {
        let records = tx.env().records(DUAL).unwrap();
        let mut row = std::collections::BTreeMap::new();
        row.insert("name".to_owned(), Value::Str("n".into()));
        records.create_values(&mut tx, &[row]).await.unwrap().ids()[0]
    };
    let err = call(&mut tx, DUAL, &[id], "rename", r#"["z", 9]"#)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(err.message().contains("orm:write"), "{}", err.message());
    tx.rollback().await.unwrap();
}

/// The typed references of the fixture, read from its own `describe()` text.
fn fixture_refs() -> Vec<erp_modules::wasm_lifecycle::DescribedRef> {
    assert_eq!(ADDON_REFS.name, "test_wasm_refs");
    let manifest = Manifest::from_toml(
        "test_wasm_refs",
        "name = 'R'\ndepends = ['test_wasm_dual_a']\n[wasm]\nsdk_version = '1.0'\n",
    )
    .unwrap();
    refs_from_describe(&manifest, &erp_sdk::describe()).unwrap()
}

#[tokio::test]
async fn the_references_match_the_installed_models() {
    let w = native_world().await;
    let refs = fixture_refs();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].model, DUAL);
    let names: Vec<&str> = refs[0].fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["name", "qty", "double_qty", "note", "loan_date"]);
    let addon = ModuleName::new("test_wasm_refs").unwrap();
    check_refs_in_registry(&addon, &refs, w.h.registry()).unwrap();
}

#[tokio::test]
async fn a_reference_to_a_missing_field_is_refused_by_name() {
    let h = Harness::builder(vec![&base_users::ADDON, &ADDON_A, &ADDON_REFS])
        .build()
        .await
        .unwrap();
    let addon = ModuleName::new("test_wasm_refs").unwrap();
    let err = check_refs_in_registry(&addon, &fixture_refs(), h.registry()).unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("test_wasm_refs")
            && text.contains("model 'test.wasm.dual' has no field 'note'"),
        "{text}"
    );
}

#[tokio::test]
async fn a_reference_of_the_wrong_type_is_refused_by_name() {
    let w = native_world().await;
    let mut refs = fixture_refs();
    // the handle says `qty` holds text
    for field in &mut refs[0].fields {
        if field.name == "qty" {
            field.kinds = vec!["char".to_owned()];
        }
    }
    let addon = ModuleName::new("test_wasm_refs").unwrap();
    let text = check_refs_in_registry(&addon, &refs, w.h.registry())
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("test.wasm.dual.qty is a integer field"),
        "{text}"
    );
}

#[test]
fn describe_carries_the_reference_kinds() {
    let doc: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    let entry = doc["contributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["addon"] == "test_wasm_refs" && e["kind"] == "model_ref")
        .expect("a model_ref entry");
    assert_eq!(entry["model"], DUAL);
    let qty = &entry["detail"]["fields"][1];
    assert_eq!(qty["name"], "qty");
    assert_eq!(qty["kinds"], serde_json::json!(["integer"]));
    assert_eq!(qty["comodel"], serde_json::Value::Null);
}
