//! A component that misbehaves toward the host (security review r1):
//! `test_wasm_forge` built for `wasm32-wasip2` runs through `erp-wasm-host` and the ORM. It lies
//! about what its `create` made, its constraint tries to write, and its method writes in whatever
//! environment it is called in; the host must refuse each, with the real activation, bridge and
//! adapter between the guest and the database.
//!
//! The tests are `#[ignore]`d in the workspace run and fail, never skip, without the artefact:
//! `scripts/ci.sh` step `wasm-dual-describe` builds the component (fat LTO) and sets
//! `ERP_WASM_FORGE_COMPONENT`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_test_wasm_forge::ADDON as ADDON_F;
use erp_addon_test_wasm_ovr_a::ADDON as ADDON_A;
use erp_fields::{Cell, ErrorKind};
use erp_orm::{
    Context, Orm, PrincipalHint, RawVals, Records, Transaction, WasmConstraintSpec,
    call_method_json, wasm_constraint_impl,
};
use erp_sdk::Value;
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_wasm_host::{
    CallableMethods, Limits, OrmCapabilities, WasmAddon, WasmHost, callable_methods,
};

const MODEL: &str = "test.wasm.ovr";

struct World {
    h: Harness,
    orm: Orm,
}

fn component_bytes() -> Vec<u8> {
    let path = std::env::var("ERP_WASM_FORGE_COMPONENT")
        .unwrap_or_else(|_| panic!("ERP_WASM_FORGE_COMPONENT must name the forge component"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

async fn world() -> World {
    world_with_limits(Limits::default()).await
}

async fn world_with_limits(limits: Limits) -> World {
    let h = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_F])
        .as_user(4)
        .build()
        .await
        .unwrap();
    {
        let mut tx = h.orm().begin(PrincipalHint::system()).await.unwrap();
        tx.batch(
            "INSERT INTO res_users (id, name, login, active) VALUES (4, 'four', 'four', true)",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let host = Arc::new(WasmHost::new().unwrap());
    let component = host.component(&component_bytes()).unwrap();
    let caps = OrmCapabilities::all();
    let probe = WasmAddon::new(
        Arc::clone(&host),
        ADDON_F.name,
        component.clone(),
        limits,
        caps,
        CallableMethods::default(),
    );
    let describe = probe.describe().await.unwrap();
    let callable = callable_methods(h.orm(), ADDON_F.name, &describe).unwrap();
    let addon = WasmAddon::new(host, ADDON_F.name, component, limits, caps, callable);
    let wasm = addon.method_impls(&describe).unwrap();
    let constraint = wasm_constraint_impl(&WasmConstraintSpec {
        addon: ADDON_F.name.to_owned(),
        model: MODEL.to_owned(),
        method: "check_forge".to_owned(),
        fields: vec!["qty".to_owned()],
    })
    .unwrap();
    let orm = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(wasm)
        .constraints([constraint])
        .build()
        .unwrap();
    World { h, orm }
}

async fn begin(w: &World) -> Transaction {
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    w.orm.begin_with(w.h.principal(), ctx).await.unwrap()
}

async fn create(tx: &mut Transaction, name: &str, qty: i32) -> erp_fields::Result<Records> {
    tx.env()
        .records(MODEL)
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("qty", Cell::Int(qty)),
            ],
        )
        .await
}

async fn name_of(tx: &mut Transaction, records: &Records) -> String {
    match records.cell(tx, "name").await.unwrap() {
        Cell::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// An honest `create` override passes the host's check and answers what was made.
#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn an_honest_create_is_accepted() {
    big_stack(async {
        let w = world().await;
        let mut tx = begin(&w).await;
        let made = create(&mut tx, "honest", 1).await.unwrap();
        assert_eq!(made.rec_ids().len(), 1);
        assert_eq!(name_of(&mut tx, &made).await, "honest");
        tx.rollback().await.unwrap();
    });
}

/// H3: the ids a create override answers must be exactly the ones its super-call made. A dropped
/// id, a repeated id and another record's id are each a validation error, and the transaction
/// cannot commit.
#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn a_create_that_lies_about_its_records_is_refused() {
    big_stack(async {
        let w = world().await;
        for lie in ["forge-drop", "forge-dup", "forge-shift"] {
            let mut tx = begin(&w).await;
            let err = create(&mut tx, lie, 1).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation, "{lie}: {err}");
            assert!(
                err.to_string().contains("must answer exactly"),
                "{lie}: {err}"
            );
            assert!(
                tx.commit().await.is_err(),
                "{lie}: the transaction must not commit"
            );
        }
    });
}

#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn a_create_that_continues_fewer_inputs_is_refused() {
    big_stack(async {
        let w = world().await;
        let mut tx = begin(&w).await;
        let model = tx.env().records(MODEL).unwrap();
        let vals = ["forge-short", "second"]
            .into_iter()
            .map(|name| {
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("qty", Cell::Int(1))
            })
            .collect();
        let err = model.create(&mut tx, vals).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
        assert_eq!(
            err.message(),
            "a create override answered 1 records for 2 input values"
        );
        let commit_err = tx.commit().await.unwrap_err();
        assert_eq!(commit_err.kind(), err.kind());
        assert_eq!(commit_err.message(), err.message());
        let mut tx = begin(&w).await;
        let err = create(&mut tx, "forge-skip", 1).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
        assert_eq!(
            err.message(),
            "a create override answered 0 records for 1 input values"
        );
        let commit_err = tx.commit().await.unwrap_err();
        assert_eq!(commit_err.kind(), err.kind());
        assert_eq!(commit_err.message(), err.message());
    });
}

#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn create_arguments_are_bounded_and_well_formed() {
    big_stack(async {
        let w = world_with_limits(Limits::default().with_payload_bytes(64).unwrap()).await;
        let mut tx = begin(&w).await;
        let model = tx.env().records(MODEL).unwrap();
        let err = call_method_json(&mut tx, &model, "create", "[")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
        // Native ABI1 preflight rejects malformed JSON before guest dispatch; its diagnostic
        // follows the shared ORM wire-form contract (wasm_abi1::json_plan, wire::bad_args).
        assert_eq!(
            err.message(),
            "arguments of 'create' do not match its wire form: EOF while parsing a list at line 1 column 1"
        );
        let oversized = format!("[[{{\"name\":\"{}\"}}]]", "x".repeat(100));
        let err = call_method_json(&mut tx, &model, "create", &oversized)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
        assert_eq!(
            err.message(),
            format!(
                "create arguments: payload bytes limit exceeded: {} > 64",
                oversized.len()
            )
        );
        tx.rollback().await.unwrap();
    });
}

/// H1: a constraint reads (its quantities) but its request to write is refused by the host, and
/// the refusal fails the write as a validation error; nothing is written.
#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn a_constraint_cannot_write() {
    big_stack(async {
        let w = world().await;
        let mut tx = begin(&w).await;
        // the constraint ran and read: an ordinary quantity passes
        let rec = create(&mut tx, "ok", 5).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = begin(&w).await;
        let err = create(&mut tx, "writer", 777).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
        assert!(err.to_string().contains("only read"), "{err}");
        tx.rollback().await.unwrap();

        let mut tx = begin(&w).await;
        let rec = tx
            .env()
            .records(MODEL)
            .unwrap()
            .browse(vec![rec.rec_ids()[0].get()])
            .unwrap();
        assert_eq!(name_of(&mut tx, &rec).await, "ok");
        tx.rollback().await.unwrap();
    });
}

/// H1: a method writes as the user in an ordinary environment; in the System environment, which
/// the guest did not ask for, the same request is refused.
#[test]
#[ignore = "needs the forge component (ERP_WASM_FORGE_COMPONENT)"]
fn an_elevated_environment_does_not_lend_its_rights_to_the_guest() {
    big_stack(async {
        let w = world().await;
        let mut tx = begin(&w).await;
        let rec = create(&mut tx, "target", 1).await.unwrap();
        let answer = call_method_json(&mut tx, &rec, "touch", "null")
            .await
            .unwrap();
        assert_eq!(answer, "\"done\"");
        assert_eq!(name_of(&mut tx, &rec).await, "touched");
        tx.commit().await.unwrap();

        let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
        let mut tx = w
            .orm
            .begin_with(PrincipalHint::system(), ctx.clone())
            .await
            .unwrap();
        let rec = tx
            .env()
            .records(MODEL)
            .unwrap()
            .browse(vec![rec.rec_ids()[0].get()])
            .unwrap();
        let err = call_method_json(&mut tx, &rec, "touch", "null")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert!(err.to_string().contains("elevated"), "{err}");
        tx.rollback().await.unwrap();

        // `sudo(true)` on an ordinary user is the same
        let mut tx = begin(&w).await;
        let rec = tx
            .env()
            .records(MODEL)
            .unwrap()
            .browse(vec![rec.rec_ids()[0].get()])
            .unwrap()
            .sudo(true);
        let err = call_method_json(&mut tx, &rec, "touch", "null")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        tx.rollback().await.unwrap();
    });
}

/// The tests drive a guest on the test's own stack: run them on a thread with room.
fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}
