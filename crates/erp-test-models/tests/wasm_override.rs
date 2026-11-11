//! Overrides of the core `create`, `write` and `unlink`, computed fields and constraints that a
//! WebAssembly addon contributes.
//!
//! The WebAssembly layer is a stand-in `MethodInvoker` written in the test, which speaks the wire
//! form a component does, so what is proven is the ORM side: the chain order with native layers
//! above and below, `super()` reaching the core implementation with the invoking identity, the
//! compute graph plugging in a compute that lives outside the binary, and a constraint failing the
//! transaction. The real component (guest code, host adapter) runs in the end-to-end proof
//! `crates/erp-cli/tests/wasm_addon_poc.rs`. Layers: `test_wasm_ovr_a` (native, bottom),
//! `test_wasm_ovr_w` (WebAssembly), `test_wasm_ovr_c` (native, top).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_test_wasm_ovr_a::{ADDON as ADDON_A, TRAIL, note};
use erp_addon_test_wasm_ovr_c::ADDON as ADDON_C;
use erp_addon_test_wasm_ovr_w::ADDON as ADDON_W;
use erp_fields::{Cell, Error, ErrorKind, Result};
use erp_orm::{
    BoxFuture, ComputeImpl, ConstraintImpl, Context, DynSuper, ImplKind, MethodInvoker, Orm,
    RawVals, Records, Transaction, WasmComputeSpec, WasmConstraintSpec, WasmMethodImpl,
    call_method, call_method_json, wasm_compute_impl, wasm_constraint_impl,
};
use erp_sdk::{Operation, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

const MODEL: &str = "test.wasm.ovr";

/// The fixtures share one trail; the tests of this file take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn trail() -> Vec<String> {
    TRAIL.lock().unwrap().clone()
}

fn clear_trail() {
    TRAIL.lock().unwrap().clear();
}

/// User 2 may read the model but not write it.
struct Rules;

impl erp_testkit::TestRules for Rules {
    fn check_model(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if model.name == MODEL && b.principal().subject().get() == 2 && op != Operation::Read {
            return Err(Error::access("denied by the test rules"));
        }
        if model.name == MODEL && b.principal().subject().get() == 3 && op == Operation::Read {
            return Err(Error::access("compute input read denied by the test rules"));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &erp_registry::ModelSpec,
        _op: Operation,
    ) -> Result<erp_sdk::Domain> {
        Ok(erp_sdk::Domain::TRUE)
    }
}

/// The WebAssembly layer: the methods of `test_wasm_ovr_w`, as a component would answer them on
/// the wire.
#[derive(Debug)]
struct Layer {
    method: &'static str,
    kind: ImplKind,
}

fn bad(text: impl Into<String>) -> Error {
    Error::validation(text)
}

impl MethodInvoker for Layer {
    fn addon(&self) -> &str {
        ADDON_W.name
    }

    fn kind(&self) -> ImplKind {
        self.kind
    }

    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            match self.method {
                "create" => {
                    note("w:create");
                    let (mut list,): (Vec<serde_json::Value>,) =
                        serde_json::from_str(args_json).unwrap();
                    for vals in &mut list {
                        vals["stamp"] = serde_json::json!("by wasm");
                    }
                    sup.call_json(tx, records, &serde_json::to_string(&(list,)).unwrap())
                        .await
                }
                "write" => {
                    note("w:write");
                    let (mut vals,): (serde_json::Value,) =
                        serde_json::from_str(args_json).unwrap();
                    if vals["qty"] == serde_json::json!(-99) {
                        return Err(bad("qty -99 is reserved"));
                    }
                    vals["stamp"] = serde_json::json!("by wasm");
                    sup.call_json(tx, records, &serde_json::to_string(&(vals,)).unwrap())
                        .await
                }
                "unlink" => {
                    note("w:unlink");
                    sup.call_json(tx, records, args_json).await
                }
                "compute_doubled" => {
                    let (batch,): (serde_json::Value,) = serde_json::from_str(args_json).unwrap();
                    let qty = batch["columns"]["qty"].as_array().unwrap();
                    let doubled: Vec<i64> = qty.iter().map(|q| q.as_i64().unwrap() * 2).collect();
                    Ok(serde_json::json!({ "doubled": doubled }).to_string())
                }
                "compute_label" => {
                    let (batch,): (serde_json::Value,) = serde_json::from_str(args_json).unwrap();
                    let names = batch["columns"]["name"].as_array().unwrap();
                    let qty = batch["columns"]["qty"].as_array().unwrap();
                    let labels: Vec<String> = names
                        .iter()
                        .zip(qty)
                        .map(|(n, q)| format!("{}:{}", n.as_str().unwrap_or("-"), q))
                        .collect();
                    Ok(serde_json::json!({ "label": labels }).to_string())
                }
                "check_qty" => {
                    // a native stand-in reading through the ORM; the real guest's read-only bridge
                    // and its identity are covered by `wasm_dispatch_security.rs` (the host's
                    // activation and adapter), which this stand-in does not go through
                    for cell in records.cells(tx, "qty").await? {
                        if matches!(cell, Cell::Int(q) if q < 0) {
                            return Err(bad("The quantity cannot be negative."));
                        }
                    }
                    Ok("null".to_owned())
                }
                other => panic!("the stand-in has no method {other}"),
            }
        })
    }
}

fn layers() -> Vec<WasmMethodImpl> {
    [
        ("create", ImplKind::Override),
        ("write", ImplKind::Override),
        ("unlink", ImplKind::Override),
        ("compute_doubled", ImplKind::Define),
        ("compute_label", ImplKind::Define),
        ("check_qty", ImplKind::Define),
    ]
    .into_iter()
    .map(|(method, kind)| WasmMethodImpl {
        model: MODEL.to_owned(),
        method: method.to_owned(),
        invoker: Arc::new(Layer { method, kind }),
    })
    .collect()
}

fn computes() -> Vec<&'static ComputeImpl> {
    let spec = |method: &str, depends: &[&str]| WasmComputeSpec {
        addon: ADDON_W.name.to_owned(),
        model: MODEL.to_owned(),
        method: method.to_owned(),
        depends: depends.iter().map(|d| (*d).to_owned()).collect(),
        reads: Vec::new(),
        depends_context: Vec::new(),
    };
    vec![
        wasm_compute_impl(&spec("compute_doubled", &["qty"])).unwrap(),
        wasm_compute_impl(&spec("compute_label", &["name", "qty"])).unwrap(),
    ]
}

fn constraints() -> Vec<&'static ConstraintImpl> {
    vec![
        wasm_constraint_impl(&WasmConstraintSpec {
            addon: ADDON_W.name.to_owned(),
            model: MODEL.to_owned(),
            method: "check_qty".to_owned(),
            fields: vec!["qty".to_owned()],
        })
        .unwrap(),
    ]
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_W, &ADDON_C])
        .rules(Rules)
        .with_computes(computes())
        .with_constraints(constraints())
        .with_wasm_methods(layers())
        .build()
        .await
        .unwrap()
}

async fn begin(orm: &Orm, h: &Harness) -> Transaction {
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    orm.begin_with(h.principal(), ctx).await.unwrap()
}

fn vals(name: &str, qty: i32) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("qty", Cell::Int(qty))
}

async fn make(tx: &mut Transaction, name: &str, qty: i32) -> Records {
    tx.env()
        .records(MODEL)
        .unwrap()
        .create(tx, vec![vals(name, qty)])
        .await
        .unwrap()
}

async fn text(tx: &mut Transaction, records: &Records, field: &str) -> String {
    match records.cell(tx, field).await.unwrap() {
        Cell::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

async fn int(tx: &mut Transaction, records: &Records, field: &str) -> i64 {
    match records.cell(tx, field).await.unwrap() {
        Cell::Int(i) => i64::from(i),
        other => panic!("{field}: {other:?}"),
    }
}

/// The order of the layers, then what each of them did to the database. Native C over WebAssembly
/// over native A over the core: create, write and unlink are entered in that order, the
/// WebAssembly layer's change reaches the row, and the typed callers see the types they asked for.
#[tokio::test]
async fn create_write_and_unlink_run_through_the_layers_in_chain_order() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;

    clear_trail();
    let created = make(&mut tx, "book", 1).await;
    assert_eq!(trail(), ["c:create", "w:create", "a:create"]);
    assert_eq!(
        created.rec_ids().len(),
        1,
        "the result crossed back as records"
    );
    assert_eq!(text(&mut tx, &created, "stamp").await, "by wasm");

    clear_trail();
    created
        .write(
            &mut tx,
            RawVals::new().set_cell("stamp", Cell::Str("hand".into())),
        )
        .await
        .unwrap();
    assert_eq!(trail(), ["c:write", "w:write", "a:write"]);
    assert_eq!(
        text(&mut tx, &created, "stamp").await,
        "by wasm",
        "the layer replaced the value before super() reached the core write"
    );

    clear_trail();
    created.unlink(&mut tx).await.unwrap();
    assert_eq!(trail(), ["c:unlink", "w:unlink", "a:unlink"]);
    let gone = tx
        .env()
        .records(MODEL)
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert!(gone.is_empty(), "the core unlink deleted the row");
    tx.rollback().await.unwrap();
}

/// The wire entry (what a component's own `call-method` uses) reaches the same chain.
#[tokio::test]
async fn the_wire_entry_runs_the_same_chain() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    let created = make(&mut tx, "wire", 1).await;

    clear_trail();
    let answer = call_method_json(&mut tx, &created, "write", r#"[{"name": "renamed"}]"#)
        .await
        .unwrap();
    assert_eq!(answer, "null");
    assert_eq!(trail(), ["c:write", "w:write", "a:write"]);
    assert_eq!(text(&mut tx, &created, "name").await, "renamed");

    clear_trail();
    let model = tx.env().records(MODEL).unwrap();
    let answer = call_method_json(&mut tx, &model, "create", r#"[[{"name": "from json"}]]"#)
        .await
        .unwrap();
    let ids: Vec<i64> = serde_json::from_str(&answer).unwrap();
    assert_eq!(ids.len(), 1, "{answer}");
    assert_eq!(trail(), ["c:create", "w:create", "a:create"]);

    // a document that does not fit the model is a validation error, not a default
    let err = call_method_json(&mut tx, &created, "write", r#"[{"no_such_field": 1}]"#)
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Programming | ErrorKind::Validation),
        "{err}"
    );
    let err = call_method_json(&mut tx, &created, "write", r#"["not an object"]"#)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    tx.rollback().await.unwrap();
}

/// `super()` from WebAssembly reaches the core write with the identity of the records it was
/// invoked on: a user without write access is refused exactly as without the layer, and nothing is
/// written.
#[tokio::test]
async fn access_denial_is_preserved_through_a_wasm_override() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'two', 'two', true)")
        .await
        .unwrap();
    let rec = make(&mut tx, "guarded", 1).await;
    tx.commit().await.unwrap();

    let mut tx = begin(h.orm(), &h).await;
    let base = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec.rec_ids()[0].get()])
        .unwrap();
    let as_two = base.with_user(2);
    clear_trail();
    let err = as_two
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("hijacked".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        trail(),
        ["c:write", "w:write", "a:write"],
        "the layers ran in order and the core refused"
    );
    tx.rollback().await.unwrap();

    let mut tx = begin(h.orm(), &h).await;
    let base = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec.rec_ids()[0].get()])
        .unwrap();
    assert_eq!(text(&mut tx, &base, "name").await, "guarded");
    // the superuser path of the same layer works: only the identity decided
    base.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("allowed".into())),
    )
    .await
    .unwrap();
    assert_eq!(text(&mut tx, &base, "name").await, "allowed");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn stored_compute_input_is_not_readable_without_model_access() {
    let _turn = TURN.lock().await;
    let h = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_W, &ADDON_C])
        .as_user(3)
        .rules(Rules)
        .with_computes(computes())
        .with_wasm_methods(layers())
        .build()
        .await
        .unwrap();
    let mut tx = h
        .orm()
        .begin(erp_orm::PrincipalHint::system())
        .await
        .unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (3, 'three', 'three', true)")
        .await
        .unwrap();
    let rec = make(&mut tx, "guarded compute", 1).await;
    tx.commit().await.unwrap();

    let mut tx = begin(h.orm(), &h).await;
    let as_three = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec.rec_ids()[0].get()])
        .unwrap();
    as_three
        .write(&mut tx, RawVals::new().set_cell("qty", Cell::Int(2)))
        .await
        .unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), "compute input read denied by the test rules");
    tx.rollback().await.unwrap();
}

/// A WebAssembly layer that refuses an edit stops the chain: nothing below it runs.
#[tokio::test]
async fn a_wasm_layer_can_refuse_an_edit() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    let rec = make(&mut tx, "edit", 1).await;
    clear_trail();
    let err = rec
        .write(&mut tx, RawVals::new().set_cell("qty", Cell::Int(-99)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(err.to_string().contains("reserved"), "{err}");
    assert_eq!(trail(), ["c:write", "w:write"], "a:write did not run");
    tx.rollback().await.unwrap();
}

/// Computed fields whose compute runs outside the binary: the stored one is filled when the
/// record is created, both follow a change of their dependencies, and the non-stored one also
/// works on a record that does not exist in the database yet.
#[tokio::test]
async fn computed_fields_follow_their_dependencies() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    let rec = make(&mut tx, "calc", 3).await;
    assert_eq!(int(&mut tx, &rec, "doubled").await, 6);
    assert_eq!(text(&mut tx, &rec, "label").await, "calc:3");

    rec.write(&mut tx, RawVals::new().set_cell("qty", Cell::Int(5)))
        .await
        .unwrap();
    assert_eq!(
        int(&mut tx, &rec, "doubled").await,
        10,
        "recomputed after the write"
    );
    assert_eq!(text(&mut tx, &rec, "label").await, "calc:5");
    rec.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("renamed".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&mut tx, &rec, "label").await,
        "renamed:5",
        "the name is a dependency too"
    );
    tx.commit().await.unwrap();

    // the stored value is what the database holds
    let mut tx = begin(h.orm(), &h).await;
    let rows = tx
        .query(
            "SELECT doubled FROM test_wasm_ovr WHERE name = 'renamed'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 10);

    // a record that is only in memory: the compute gets its values as data
    let model = tx.env().records(MODEL).unwrap();
    let new = tx
        .new_record(&model, vals("fresh", 4), None, None)
        .await
        .unwrap();
    assert!(new.rec_ids()[0].is_new());
    assert_eq!(text(&mut tx, &new, "label").await, "fresh:4");
    tx.rollback().await.unwrap();
}

/// A constraint of the layer runs after a write or a create touched its fields and fails the
/// transaction: the commit is refused and the old value is still there afterwards.
#[tokio::test]
async fn a_constraint_of_the_layer_rolls_the_transaction_back() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    let rec = make(&mut tx, "limit", 2).await;
    tx.commit().await.unwrap();

    let mut tx = begin(h.orm(), &h).await;
    let rec = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec.rec_ids()[0].get()])
        .unwrap();
    let err = rec
        .write(&mut tx, RawVals::new().set_cell("qty", Cell::Int(-1)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(err.to_string().contains("negative"), "{err}");
    assert!(
        tx.commit().await.is_err(),
        "a transaction that failed a constraint cannot commit"
    );

    let mut tx = begin(h.orm(), &h).await;
    let rec = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec.rec_ids()[0].get()])
        .unwrap();
    assert_eq!(
        int(&mut tx, &rec, "qty").await,
        2,
        "the write was rolled back"
    );
    // a create that breaks it fails too
    let err = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(&mut tx, vec![vals("neg", -5)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    tx.rollback().await.unwrap();
}

/// The typed entry crosses the seam both ways too (native code calling `create` and `write`
/// through `call_method` with the typed argument tuples).
#[tokio::test]
async fn the_typed_entry_crosses_the_seam() {
    let _turn = TURN.lock().await;
    let h = harness().await;
    let mut tx = begin(h.orm(), &h).await;
    let model = tx.env().records(MODEL).unwrap();
    let created: Records = call_method(&mut tx, &model, "create", (vec![vals("typed", 7)],))
        .await
        .unwrap();
    assert_eq!(created.rec_ids().len(), 1);
    let () = call_method(
        &mut tx,
        &created,
        "write",
        (RawVals::new().set_cell("qty", Cell::Int(8)),),
    )
    .await
    .unwrap();
    assert_eq!(int(&mut tx, &created, "doubled").await, 16);
    tx.rollback().await.unwrap();
}

/// Names and counts a package supplies are checked before anything is interned.
#[test]
fn a_compute_with_a_bad_name_or_a_path_is_refused() {
    let base = WasmComputeSpec {
        addon: ADDON_W.name.to_owned(),
        model: MODEL.to_owned(),
        method: "compute_ok".to_owned(),
        depends: vec!["qty".to_owned()],
        reads: Vec::new(),
        depends_context: Vec::new(),
    };
    wasm_compute_impl(&base).unwrap();
    for bad in [
        WasmComputeSpec {
            method: "no space".to_owned(),
            ..base.clone()
        },
        WasmComputeSpec {
            model: "Upper.Case".to_owned(),
            ..base.clone()
        },
        WasmComputeSpec {
            depends: vec!["lines.qty".to_owned()],
            ..base.clone()
        },
        WasmComputeSpec {
            reads: vec![String::new()],
            ..base.clone()
        },
    ] {
        let err = wasm_compute_impl(&bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    }
    let constraint = WasmConstraintSpec {
        addon: ADDON_W.name.to_owned(),
        model: MODEL.to_owned(),
        method: "check_x".to_owned(),
        fields: vec!["a.b".to_owned()],
    };
    assert!(wasm_constraint_impl(&constraint).is_err());
    // the same spec twice is the same implementation
    let again = wasm_compute_impl(&base).unwrap();
    assert!(std::ptr::eq(again, wasm_compute_impl(&base).unwrap()));
}

/// Security review r1 H2: a compute of a package cannot name a field that is restricted to groups.
#[tokio::test]
async fn a_package_compute_cannot_read_a_protected_field() {
    let spec = WasmComputeSpec {
        addon: ADDON_W.name.to_owned(),
        model: MODEL.to_owned(),
        method: "compute_doubled".to_owned(),
        depends: vec!["secret".to_owned()],
        reads: Vec::new(),
        depends_context: Vec::new(),
    };
    let bad = wasm_compute_impl(&spec).unwrap();
    let err = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_W, &ADDON_C])
        .with_computes([bad])
        .with_constraints(constraints())
        .with_wasm_methods(layers())
        .build()
        .await
        .err()
        .expect("the build must refuse");
    let text = err.to_string();
    assert!(
        text.contains("secret") && text.contains("protected"),
        "{text}"
    );
}
