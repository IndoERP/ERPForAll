//! Security review r1: what a guest may
//! reach through the adapter, and as whom it runs.
//!
//! No guest code runs here. A stand-in `MethodInvoker` sits where the component would and sends one
//! request through `erp_wasm_host::probe_request`, which is the adapter the real dispatcher runs
//! (same environment, identity and checks). The component is an empty one: the tests need a
//! `WasmAddon` but never enter a guest, so they run in the ordinary workspace run. The real
//! component, with real guest code, is covered by `wasm_dispatch_component.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use erp_addon_test_wasm_dual_a::ADDON as ADDON_A;
use erp_addon_test_wasm_dual_b::ADDON as ADDON_B;
use erp_fields::{Error, Result};
use erp_orm::rpc::Exposure;
use erp_orm::{
    BeginOptions, BoxFuture, CompanyId, Context, DynSuper, GroupSet, ImplKind, MethodInvoker, Orm,
    PrincipalBuilder, Records, Resolution, Transaction, UserId, WasmMethodImpl, call_method_json,
};
use erp_sdk::{Domain, Operation, Operator, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_wasm_host::{
    BoundaryErrorKind, CallableMethods, Limits, MethodRole, OrmCapabilities, OrmOp, ProbeRequest,
    WasmAddon, WasmHost, callable_methods, probe_request_as,
};

const MODEL: &str = "test.wasm.dual";
/// A component with no imports, exports or instances: valid, and never entered.
const EMPTY_COMPONENT: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

/// User 2 may not touch the model at all; user 3 sees only the records named `visible`.
struct Rules;

impl erp_testkit::TestRules for Rules {
    /// The transaction's user (4) invokes the guest through records switched to users 2 and 3:
    /// it holds `principal.impersonate` here (A4); no other capability is mapped.
    fn has_capability(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        cap: &erp_orm::Capability,
    ) -> Result<bool> {
        if cap.name() == "principal.impersonate" {
            return Ok(b.principal().subject().get() == 4);
        }
        Err(Error::programming(format!(
            "the dispatch fixture maps no capability '{}'",
            cap.name()
        )))
    }

    fn check_model(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        if model.name == MODEL && b.principal().subject().get() == 2 {
            return Err(Error::access("denied by the test rules"));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        if model.name == MODEL && b.principal().subject().get() == 3 {
            return Ok(Domain::condition("name", Operator::Eq, "visible").unwrap());
        }
        Ok(Domain::TRUE)
    }
}

type Answer = std::result::Result<String, (BoundaryErrorKind, String)>;

/// The stand-in layer: runs the request it was given through the adapter.
struct ProbeLayer {
    addon: Arc<WasmAddon>,
    request: Mutex<Option<ProbeRequest>>,
    answer: Mutex<Option<Answer>>,
    role: Mutex<MethodRole>,
}

impl std::fmt::Debug for ProbeLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProbeLayer")
    }
}

impl MethodInvoker for ProbeLayer {
    fn addon(&self) -> &str {
        ADDON_B.name
    }

    fn kind(&self) -> ImplKind {
        ImplKind::Override
    }

    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        _args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let request = self.request.lock().unwrap().take().expect("a request");
            let role = *self.role.lock().unwrap();
            let answer = probe_request_as(tx, &self.addon, records, sup, role, request)
                .await
                .map_err(|e| (e.kind, e.message));
            *self.answer.lock().unwrap() = Some(answer);
            Ok("\"ok\"".to_owned())
        })
    }
}

struct World {
    h: Harness,
    orm: Orm,
    layer: Arc<ProbeLayer>,
}

fn caps_all() -> OrmCapabilities {
    OrmCapabilities::NONE
        .with(OrmOp::Read)
        .with(OrmOp::Write)
        .with(OrmOp::CallMethod)
}

async fn world(callable: impl FnOnce(&Orm) -> CallableMethods) -> World {
    // the transaction's own identity is an ordinary user (4): in a System or elevated environment
    // a guest may only continue its chain (security review r1 H1)
    let h = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_B])
        .rules(Rules)
        // user 4 switches to the users the guest runs as (A4)
        .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
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
            "INSERT INTO res_users (id, name, login, active) VALUES \
             (2, 'two', 'two', true), (3, 'three', 'three', true), (4, 'four', 'four', true)",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let host = Arc::new(WasmHost::new().unwrap());
    let component = host.component(&EMPTY_COMPONENT).unwrap();
    let callable = callable(h.orm());
    let addon = WasmAddon::new(
        host,
        ADDON_B.name,
        component,
        Limits::default(),
        caps_all(),
        callable,
    );
    let layer = Arc::new(ProbeLayer {
        addon,
        request: Mutex::new(None),
        answer: Mutex::new(None),
        role: Mutex::new(MethodRole::Method),
    });
    let orm = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(vec![WasmMethodImpl {
            model: MODEL.to_owned(),
            method: "label".to_owned(),
            invoker: layer.clone(),
        }])
        .build()
        .unwrap();
    World { h, orm, layer }
}

/// A description whose `method` contributions the addon "declares".
fn describe(methods: &[(&str, &str)]) -> String {
    let list: Vec<serde_json::Value> = methods
        .iter()
        .map(|(model, name)| {
            serde_json::json!({
                "addon": ADDON_B.name, "kind": "method", "model": model, "name": name,
                "detail": {"args": []},
            })
        })
        .collect();
    serde_json::json!({"schema": 1, "contributions": list}).to_string()
}

async fn begin(w: &World) -> Transaction {
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    w.orm.begin_with(w.h.principal(), ctx).await.unwrap()
}

async fn make(tx: &mut Transaction, name: &str) -> i64 {
    let mut row = BTreeMap::new();
    row.insert("name".to_owned(), Value::Str(name.into()));
    tx.env()
        .records(MODEL)
        .unwrap()
        .create_values(tx, &[row])
        .await
        .unwrap()
        .ids()[0]
}

/// Creates one record per name and commits, so the transactions that follow start with an empty
/// cache and every read goes through the access checks.
async fn seed(w: &World, names: &[&str]) -> Vec<i64> {
    let mut tx = begin(w).await;
    let mut ids = Vec::new();
    for name in names {
        ids.push(make(&mut tx, name).await);
    }
    tx.commit().await.unwrap();
    ids
}

/// Sends `request` through the adapter while the method `label` runs on `records`.
async fn probe(
    w: &World,
    tx: &mut Transaction,
    records: &Records,
    request: ProbeRequest,
) -> Answer {
    *w.layer.request.lock().unwrap() = Some(request);
    call_method_json(tx, records, "label", r#"["x"]"#)
        .await
        .unwrap();
    w.layer.answer.lock().unwrap().take().expect("an answer")
}

fn call(model: &str, method: &str, ids: &[i64]) -> ProbeRequest {
    ProbeRequest::Call {
        model: model.to_owned(),
        method: method.to_owned(),
        ids: ids.to_vec(),
        args_json: "[]".to_owned(),
    }
}

fn denied(answer: &Answer) -> &str {
    match answer {
        Err((BoundaryErrorKind::AccessDenied, message)) => message,
        other => panic!("expected access-denied, got {other:?}"),
    }
}

#[tokio::test]
async fn a_hidden_method_is_not_callable() {
    let w = world(|orm| callable_methods(orm, ADDON_B.name, &describe(&[])).unwrap()).await;
    let mut tx = begin(&w).await;
    let model_id = tx.env().records(MODEL).unwrap().model_id();
    assert!(
        matches!(
            tx.exposure().get(model_id, "hidden_probe"),
            Some(Exposure::Internal)
        ),
        "the fixture method is internal"
    );
    let id = make(&mut tx, "n").await;
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let answer = probe(&w, &mut tx, &records, call(MODEL, "hidden_probe", &[id])).await;
    assert!(denied(&answer).contains("not exposed"), "{answer:?}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_name_declared_on_another_model_does_not_unlock_the_native_method() {
    let describe = describe(&[("some.other.model", "hidden_probe")]);
    let w = world(|orm| callable_methods(orm, ADDON_B.name, &describe).unwrap()).await;
    let callable = callable_methods(w.h.orm(), ADDON_B.name, &describe).unwrap();
    // the list is keyed by model: the declared pair is in it, the native one is not
    assert!(callable.contains("some.other.model", "hidden_probe"));
    assert!(!callable.contains(MODEL, "hidden_probe"));
    // and the adapter refuses it even if a list named the pair
    let mut tx = begin(&w).await;
    let id = make(&mut tx, "n").await;
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let answer = probe(&w, &mut tx, &records, call(MODEL, "hidden_probe", &[id])).await;
    assert!(denied(&answer).contains("not exposed"), "{answer:?}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn an_exposed_method_is_callable() {
    let describe = describe(&[(MODEL, "shout")]);
    let w = world(|orm| callable_methods(orm, ADDON_B.name, &describe).unwrap()).await;
    let mut tx = begin(&w).await;
    let model_id = tx.env().records(MODEL).unwrap().model_id();
    let exposed: Vec<&str> = tx
        .exposure()
        .methods(model_id)
        .into_iter()
        .filter(|(_, e)| matches!(e, Exposure::Exposed(_)))
        .map(|(name, _)| name)
        .collect();
    assert!(exposed.contains(&"default_get"), "exposed: {exposed:?}");
    let id = make(&mut tx, "n").await;
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let request = ProbeRequest::Call {
        model: MODEL.to_owned(),
        method: "default_get".to_owned(),
        ids: vec![],
        args_json: r#"[["qty"]]"#.to_owned(),
    };
    // the builtin has no wire form, so the dispatcher refuses it further down; what matters is
    // that the exposure gate let it through
    let answer = probe(&w, &mut tx, &records, request).await;
    assert!(
        !matches!(&answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    assert!(
        matches!(&answer, Err((BoundaryErrorKind::InvalidArgument, m)) if m.contains("through its wire form")),
        "the call reached the dispatcher: {answer:?}"
    );
    // the addon's own definition passes the exposure check (the chain has no `shout` in this
    // fixture ORM, so the call then fails for another reason than access)
    let answer = probe(&w, &mut tx, &records, call(MODEL, "shout", &[id])).await;
    assert!(
        !matches!(&answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn super_call_is_limited_to_the_invoked_records_and_method() {
    let w = world(|_| CallableMethods::default()).await;
    let mut tx = begin(&w).await;
    let a = make(&mut tx, "a").await;
    let b = make(&mut tx, "b").await;
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let sup = |ids: &[i64], method: &str| ProbeRequest::Super {
        model: MODEL.to_owned(),
        method: method.to_owned(),
        ids: ids.to_vec(),
        args_json: r#"["x"]"#.to_owned(),
    };
    // a foreign id is refused
    let answer = probe(&w, &mut tx, &records, sup(&[a, b], "label")).await;
    assert!(denied(&answer).contains(&b.to_string()), "{answer:?}");
    let answer = probe(&w, &mut tx, &records, sup(&[b], "label")).await;
    denied(&answer);
    // another method is refused
    let answer = probe(&w, &mut tx, &records, sup(&[a], "shout")).await;
    denied(&answer);
    // its own records and its own method run the layer below
    let answer = probe(&w, &mut tx, &records, sup(&[a], "label")).await;
    assert_eq!(answer.unwrap(), "\"xa\"");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_guest_runs_as_the_invoking_user() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["a"]).await[0];
    let mut tx = begin(&w).await;
    let read = |fields: &[&str]| ProbeRequest::Read {
        model: MODEL.to_owned(),
        ids: vec![a],
        fields: fields.iter().map(|f| (*f).to_owned()).collect(),
    };
    let base = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    // a user the model refuses: the guest is refused as that user (asked first, before any read
    // of the same record by another identity has filled the cache)
    let user2 = base.with_user(2);
    let answer = probe(&w, &mut tx, &user2, read(&["name"])).await;
    assert!(
        matches!(answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    // the transaction's own identity may read the same record
    assert!(probe(&w, &mut tx, &base, read(&["name"])).await.is_ok());
    // reading `id` is access-checked too
    let answer = probe(&w, &mut tx, &user2, read(&["id"])).await;
    assert!(
        matches!(answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    // a sudo caller does not hand its elevation to the guest's own requests (security review r1
    // H1); the guest may only continue its chain there
    let answer = probe(&w, &mut tx, &user2.sudo(true), read(&["name"])).await;
    assert!(denied(&answer).contains("elevated"), "{answer:?}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_record_rule_of_the_invoking_user_applies() {
    let w = world(|_| CallableMethods::default()).await;
    let ids = seed(&w, &["hidden", "visible"]).await;
    let (hidden, visible) = (ids[0], ids[1]);
    let mut tx = begin(&w).await;
    let read = |id: i64| ProbeRequest::Read {
        model: MODEL.to_owned(),
        ids: vec![id],
        fields: vec!["name".to_owned()],
    };
    let user3 = |id: i64| {
        let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
        records.with_user(3)
    };
    let (rec_hidden, rec_visible) = (user3(hidden), user3(visible));
    let answer = probe(&w, &mut tx, &rec_hidden, read(hidden)).await;
    assert!(
        matches!(answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    let answer = probe(&w, &mut tx, &rec_visible, read(visible)).await;
    assert!(answer.is_ok(), "{answer:?}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_company_outside_the_users_companies_is_refused() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["visible"]).await[0];
    // user 3 belongs to company 1 only
    let company = CompanyId::new(1).unwrap();
    let resolution =
        Resolution::in_id_order(GroupSet::default(), [company], NonZeroU64::MIN).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(3).unwrap())
        .resolution(resolution)
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = w
        .orm
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let answer = probe(&w, &mut tx, &records, ProbeRequest::Identity).await;
    assert_eq!(answer.unwrap(), "3:1:[1]");
    let foreign = records.with_context([("allowed_company_ids", Value::list([Value::Int(2)]))]);
    let answer = probe(&w, &mut tx, &foreign, ProbeRequest::Identity).await;
    assert!(
        matches!(answer, Err((BoundaryErrorKind::AccessDenied, _))),
        "{answer:?}"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_callers_language_and_context_reach_the_guest() {
    let w = world(|_| CallableMethods::default()).await;
    let mut tx = begin(&w).await;
    let a = make(&mut tx, "a").await;
    let base = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    // no language in the context: the guest is told so (empty), not "en_US"
    let answer = probe(&w, &mut tx, &base, ProbeRequest::Context).await;
    assert_eq!(answer.unwrap(), "|{}");
    // the records' own context is what the guest sees
    let with = base.with_context([
        ("lang", Value::Str("fr_FR".into())),
        ("foo", Value::Int(7)),
        ("uid", Value::Int(1)),
    ]);
    let answer = probe(&w, &mut tx, &with, ProbeRequest::Context).await;
    assert_eq!(answer.unwrap(), r#"fr_FR|{"foo":7}"#);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_callable_set_and_the_layers_come_from_one_list() {
    let text = describe(&[(MODEL, "shout"), (MODEL, "other_defined")]);
    let w = world(|orm| callable_methods(orm, ADDON_B.name, &text).unwrap()).await;
    let described = erp_wasm_host::described_methods(ADDON_B.name, &text).unwrap();
    let from_list = erp_wasm_host::callable_from(w.h.orm(), &described);
    assert_eq!(
        from_list,
        callable_methods(w.h.orm(), ADDON_B.name, &text).unwrap()
    );
    assert!(from_list.contains(MODEL, "other_defined"));
    // a list that differs from the text gives a different set: the set follows the list it is given
    let shorter = erp_wasm_host::callable_from(w.h.orm(), &described[..1]);
    assert!(!shorter.contains(MODEL, "other_defined"));

    let host = std::sync::Arc::new(WasmHost::new().unwrap());
    let component = host.component(&EMPTY_COMPONENT).unwrap();
    let addon = WasmAddon::new(
        host,
        ADDON_B.name,
        component,
        Limits::default(),
        OrmCapabilities::all(),
        from_list,
    );
    let impls = addon.method_impls_from(&described);
    let pairs: Vec<(String, String)> = impls
        .iter()
        .map(|i| (i.model.clone(), i.method.clone()))
        .collect();
    let listed: Vec<(String, String)> = described
        .iter()
        .map(|d| (d.model.clone(), d.method.clone()))
        .collect();
    assert_eq!(pairs, listed);
}

fn read_name(id: i64) -> ProbeRequest {
    ProbeRequest::Read {
        model: MODEL.to_owned(),
        ids: vec![id],
        fields: vec!["name".to_owned()],
    }
}

fn write_name(id: i64) -> ProbeRequest {
    ProbeRequest::WriteText {
        model: MODEL.to_owned(),
        ids: vec![id],
        field: "name".to_owned(),
        value: "changed".to_owned(),
    }
}

/// Security review r1 H1: a compute is handed its inputs as data and asks the host for nothing.
#[tokio::test]
async fn a_compute_activation_asks_the_host_for_nothing() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["a"]).await[0];
    let mut tx = begin(&w).await;
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    *w.layer.role.lock().unwrap() = MethodRole::Compute;
    for request in [
        read_name(a),
        write_name(a),
        ProbeRequest::Search {
            model: MODEL.to_owned(),
            domain_json: "[]".to_owned(),
        },
    ] {
        let answer = probe(&w, &mut tx, &records, request).await;
        assert!(denied(&answer).contains("refused"), "{answer:?}");
    }
    tx.rollback().await.unwrap();
}

/// Security review r1 H1: a constraint may read, nothing else.
#[tokio::test]
async fn a_constraint_activation_may_only_read() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["a"]).await[0];
    let mut tx = begin(&w).await;
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    *w.layer.role.lock().unwrap() = MethodRole::Constraint;
    assert!(probe(&w, &mut tx, &records, read_name(a)).await.is_ok());
    let search = ProbeRequest::Search {
        model: MODEL.to_owned(),
        domain_json: "[]".to_owned(),
    };
    assert!(probe(&w, &mut tx, &records, search).await.is_ok());
    for request in [
        write_name(a),
        ProbeRequest::Unlink {
            model: MODEL.to_owned(),
            ids: vec![a],
        },
        call(MODEL, "label", &[a]),
    ] {
        let answer = probe(&w, &mut tx, &records, request).await;
        assert!(denied(&answer).contains("only read"), "{answer:?}");
    }
    // an elevated environment gives a constraint nothing at all
    let elevated = records.sudo(true);
    let answer = probe(&w, &mut tx, &elevated, read_name(a)).await;
    assert_eq!(
        denied(&answer),
        "`read-columns` is refused in this call: a compute may not access the host; a constraint in a sudo or System environment may not access the host"
    );
    tx.rollback().await.unwrap();
}

/// Security review r1 H1: in a `sudo` or System environment a guest may only continue its chain.
#[tokio::test]
async fn an_elevated_environment_allows_only_super_call() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["a"]).await[0];
    let mut tx = begin(&w).await;
    let plain = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let sudo = plain.sudo(true);
    for request in [read_name(a), write_name(a)] {
        let answer = probe(&w, &mut tx, &sudo, request).await;
        assert!(denied(&answer).contains("elevated"), "{answer:?}");
    }
    let sup = ProbeRequest::Super {
        model: MODEL.to_owned(),
        method: "label".to_owned(),
        ids: vec![a],
        args_json: r#"["x"]"#.to_owned(),
    };
    assert_eq!(probe(&w, &mut tx, &sudo, sup).await.unwrap(), "\"xa\"");
    tx.rollback().await.unwrap();

    // the System principal: the same
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let mut tx = w
        .orm
        .begin_with(erp_orm::PrincipalHint::system(), ctx)
        .await
        .unwrap();
    let system = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let answer = probe(&w, &mut tx, &system, read_name(a)).await;
    assert!(denied(&answer).contains("elevated"), "{answer:?}");
    tx.rollback().await.unwrap();
}

/// Security review r1 M1: only the plain System principal runs a guest without a company; a user
/// whose environment names none is refused as before.
#[tokio::test]
async fn only_the_system_principal_runs_without_a_company() {
    let w = world(|_| CallableMethods::default()).await;
    let a = seed(&w, &["a"]).await[0];
    // System, no company in the context: identity 1, company 0, no allowed companies
    let mut tx = w.orm.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let answer = probe(&w, &mut tx, &records, ProbeRequest::Identity).await;
    assert_eq!(answer.unwrap(), "1:0:[]");
    tx.rollback().await.unwrap();
    // an ordinary user with no company in the environment
    let mut tx = w.orm.begin(w.h.principal()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap().browse([a]).unwrap();
    let answer = probe(&w, &mut tx, &records, ProbeRequest::Identity).await;
    assert!(
        matches!(&answer, Err((BoundaryErrorKind::AccessDenied, m)) if m.contains("needs a company")),
        "{answer:?}"
    );
    tx.rollback().await.unwrap();
}

/// X5: the real WASM request adapter refuses the native notification by name.
/// This uses the established adapter probe; no guest component executes in this binary.
#[tokio::test]
async fn external_notification_is_not_callable_from_wasm() {
    let w = world(|orm| callable_methods(orm, ADDON_B.name, &describe(&[])).unwrap()).await;
    let mut tx = begin(&w).await;
    let id = make(&mut tx, "visible").await;
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let answer = probe(
        &w,
        &mut tx,
        &records,
        call(MODEL, "modified_external", &[id]),
    )
    .await;
    assert_eq!(
        denied(&answer),
        "method `modified_external` is not exposed on `test.wasm.dual`"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    w.h.cleanup().await.unwrap();
}
