//! Mixed native / WebAssembly override chains.
//!
//! The WebAssembly layer here is a stand-in `MethodInvoker` written in the test, so the chain logic
//! of `erp-orm` is proven without a component: the same code runs against the real component in
//! `wasm_dispatch_component.rs`. Layers: `test_wasm_dual_a` (native, defines `label`),
//! `test_wasm_dual_b` (WebAssembly, overrides `label`, defines `shout`), `test_wasm_dual_c` (native,
//! overrides both). Every layer appends its own `MARK` to what the layers below return, so a result
//! reads the order of the layers; the marks are the fixtures' own constants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::{Arc, Mutex};

use erp_addon_test_wasm_dual_a::{ADDON as ADDON_A, MARK as MARK_A};
use erp_addon_test_wasm_dual_b::{ADDON as ADDON_B, MARK as MARK_B};
use erp_addon_test_wasm_dual_c::{ADDON as ADDON_C, MARK as MARK_C};
use erp_orm::{
    BoxFuture, DynSuper, ImplKind, MethodDispatchTarget, MethodInvoker, Orm, Records, Transaction,
    WasmMethodImpl, call_method, call_method_json,
};
use erp_sdk::Result;
use erp_test_models::base_users;
use erp_testkit::Harness;

const MODEL: &str = "test.wasm.dual";

/// A WebAssembly layer written in Rust: runs the same wire protocol a component does, and records
/// when it was entered and left.
#[derive(Debug)]
struct StandIn {
    method: &'static str,
    kind: ImplKind,
    log: Arc<Mutex<Vec<String>>>,
}

impl MethodInvoker for StandIn {
    fn addon(&self) -> &str {
        ADDON_B.name
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
            self.log
                .lock()
                .unwrap()
                .push(format!("enter {} {args_json}", self.method));
            let answer = match self.method {
                "label" => {
                    // an override: the wire arguments go down unchanged
                    let inner = sup.call_json(tx, records, args_json).await?;
                    let inner: String = serde_json::from_str(&inner).unwrap();
                    serde_json::to_string(&format!("{inner}{MARK_B}")).unwrap()
                }
                "shout" => {
                    // a definition: nothing below, so no super
                    assert!(!sup.has_next());
                    let (text,): (String,) = serde_json::from_str(args_json).unwrap();
                    serde_json::to_string(&text.to_uppercase()).unwrap()
                }
                other => panic!("the stand-in has no method {other}"),
            };
            self.log
                .lock()
                .unwrap()
                .push(format!("leave {}", self.method));
            Ok(answer)
        })
    }
}

fn stand_ins(
    methods: &[(&'static str, ImplKind)],
    log: &Arc<Mutex<Vec<String>>>,
) -> Vec<WasmMethodImpl> {
    methods
        .iter()
        .map(|(method, kind)| WasmMethodImpl {
            model: MODEL.to_owned(),
            method: (*method).to_owned(),
            invoker: Arc::new(StandIn {
                method,
                kind: *kind,
                log: Arc::clone(log),
            }),
        })
        .collect()
}

async fn harness(with_c: bool) -> Harness {
    let mut addons = vec![&base_users::ADDON, &ADDON_A, &ADDON_B];
    if with_c {
        addons.push(&ADDON_C);
    }
    Harness::builder(addons).build().await.unwrap()
}

/// An ORM whose `test_wasm_dual_b` layer is `wasm`.
fn with_wasm(h: &Harness, wasm: Vec<WasmMethodImpl>) -> erp_fields::Result<Orm> {
    h.orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(wasm)
        .build()
}

const ALL: [(&str, ImplKind); 2] = [("label", ImplKind::Override), ("shout", ImplKind::Define)];

#[tokio::test]
async fn native_over_wasm_over_native_runs_in_chain_order() {
    let h = harness(true).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let orm = with_wasm(&h, stand_ins(&ALL, &log)).unwrap();
    let mut tx = orm.begin(h.principal()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap();

    let got: String = call_method(&mut tx, &records, "label", ("x".to_owned(),))
        .await
        .unwrap();
    // A defines, B wraps it, C wraps that: the marks read bottom to top
    assert_eq!(got, format!("x{MARK_A}{MARK_B}{MARK_C}"));
    // the WebAssembly layer was entered once, with the wire form of the typed arguments, and left
    // after the native layer below it had run
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            format!("enter label {}", serde_json::to_string(&("x",)).unwrap()),
            "leave label".to_owned()
        ]
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn wasm_over_native_runs_and_super_reaches_the_native_definition() {
    let h = harness(false).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let orm = with_wasm(&h, stand_ins(&ALL, &log)).unwrap();
    let mut tx = orm.begin(h.principal()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap();

    let got: String = call_method(&mut tx, &records, "label", ("x".to_owned(),))
        .await
        .unwrap();
    assert_eq!(got, format!("x{MARK_A}{MARK_B}"));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn native_over_a_wasm_definition_and_the_wire_entry() {
    let h = harness(true).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let orm = with_wasm(&h, stand_ins(&ALL, &log)).unwrap();
    let mut tx = orm.begin(h.principal()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap();

    let typed: String = call_method(&mut tx, &records, "shout", ("hello".to_owned(),))
        .await
        .unwrap();
    assert_eq!(typed, format!("HELLO{MARK_C}"));
    let wire = call_method_json(&mut tx, &records, "shout", r#"["hello"]"#)
        .await
        .unwrap();
    let wire: String = serde_json::from_str(&wire).unwrap();
    assert_eq!(wire, typed, "both entries run the same chain");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_dispatch_table_orders_native_and_wasm_entries_like_the_addons() {
    let h = harness(true).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let orm = with_wasm(&h, stand_ins(&ALL, &log)).unwrap();
    let model = orm.registry().model(MODEL).unwrap().id;
    let layers = |method: &str| -> Vec<(String, bool, ImplKind)> {
        orm.dispatch()
            .targets(model, method)
            .iter()
            .map(|t| (t.addon().to_owned(), t.is_wasm(), t.kind()))
            .collect()
    };
    assert_eq!(
        layers("label"),
        vec![
            (ADDON_C.name.to_owned(), false, ImplKind::Override),
            (ADDON_B.name.to_owned(), true, ImplKind::Override),
            (ADDON_A.name.to_owned(), false, ImplKind::Define),
        ]
    );
    assert_eq!(
        layers("shout"),
        vec![
            (ADDON_C.name.to_owned(), false, ImplKind::Override),
            (ADDON_B.name.to_owned(), true, ImplKind::Define),
        ]
    );
    // the same order the registry resolves the addons in
    let mro: Vec<&str> = orm
        .registry()
        .model(MODEL)
        .unwrap()
        .mro
        .iter()
        .map(|d| d.addon)
        .collect();
    let chain_addons: Vec<String> = layers("label").into_iter().map(|l| l.0).collect();
    let mut expected: Vec<&str> = mro
        .iter()
        .copied()
        .filter(|a| chain_addons.iter().any(|c| c == a))
        .collect();
    expected.dedup();
    assert_eq!(
        chain_addons.iter().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
    assert!(matches!(
        orm.dispatch().targets(model, "label")[1],
        MethodDispatchTarget::Wasm(_)
    ));
}

#[tokio::test]
async fn a_wasm_layer_over_a_native_method_without_wire_is_refused_at_build() {
    let h = harness(false).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    // `default_get` is core's and has no wire form (`create`, `write` and `unlink` have one since,
    // see `wasm_override.rs`)
    let wasm = vec![WasmMethodImpl {
        model: MODEL.to_owned(),
        method: "default_get".to_owned(),
        invoker: Arc::new(StandIn {
            method: "label",
            kind: ImplKind::Override,
            log,
        }),
    }];
    let err = with_wasm(&h, wasm).err().expect("build must refuse");
    let text = err.to_string();
    assert!(text.contains("default_get"), "{text}");
    assert!(text.contains("#[method(wire)]"), "{text}");
}

#[tokio::test]
async fn an_error_from_the_wasm_layer_reaches_the_native_caller() {
    #[derive(Debug)]
    struct Failing;
    impl MethodInvoker for Failing {
        fn addon(&self) -> &str {
            ADDON_B.name
        }
        fn kind(&self) -> ImplKind {
            ImplKind::Override
        }
        fn invoke<'a>(
            &'a self,
            _tx: &'a mut Transaction,
            _records: &'a Records,
            _args_json: &'a str,
            _sup: DynSuper,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Err(erp_sdk::Error::validation("no thanks")) })
        }
    }
    let h = harness(true).await;
    let mut wasm = stand_ins(&[("shout", ImplKind::Define)], &Arc::default());
    wasm.push(WasmMethodImpl {
        model: MODEL.to_owned(),
        method: "label".to_owned(),
        invoker: Arc::new(Failing),
    });
    let orm = with_wasm(&h, wasm).unwrap();
    let mut tx = orm.begin(h.principal()).await.unwrap();
    let records = tx.env().records(MODEL).unwrap();
    let err = call_method::<_, String>(&mut tx, &records, "label", ("x".to_owned(),))
        .await
        .expect_err("the failure must surface");
    assert_eq!(err.message(), "no thanks");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn invalid_names_and_undeclared_models_fail_the_build() {
    let h = harness(false).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let with = |model: &str, method: &str| {
        let mut impls = stand_ins(&[("shout", ImplKind::Define)], &log);
        impls[0].model = model.to_owned();
        impls[0].method = method.to_owned();
        with_wasm(&h, impls)
    };
    for (model, method) in [
        (MODEL, "bad name"),
        (MODEL, ""),
        (MODEL, "a.b"),
        ("Test.Wasm.Dual", "shout"),
        ("test wasm", "shout"),
        (MODEL, &"m".repeat(129)),
    ] {
        let err = with(model, method).err().expect(method);
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming, "{err}");
        assert!(err.message().contains("invalid name"), "{}", err.message());
    }
    // a well-formed name for a model the addon does not declare
    let err = with("test.wasm.undeclared", "shout")
        .err()
        .expect("undeclared");
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert!(
        err.message().contains("no declaration of model"),
        "{}",
        err.message()
    );
    // and the good one still builds
    with(MODEL, "shout").expect("valid");
}

#[tokio::test]
async fn a_refused_build_interns_no_name() {
    let h = harness(false).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut impls = stand_ins(&[("shout", ImplKind::Define)], &log);
    let mut first = stand_ins(&[("shout", ImplKind::Define)], &log);
    first[0].method = "twopass_probe_name_one".to_owned();
    let mut bad = stand_ins(&[("shout", ImplKind::Define)], &log);
    bad[0].method = "bad name".to_owned();
    impls.clear();
    impls.extend(first);
    impls.extend(bad);
    let err = with_wasm(&h, impls).err().expect("second entry is bad");
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming, "{err}");
    assert!(err.message().contains("invalid name"), "{}", err.message());
    assert!(
        !erp_orm::wasm_method_name_is_interned("twopass_probe_name_one"),
        "the valid first entry must not have been interned by a refused build"
    );

    let many: Vec<WasmMethodImpl> = (0..=erp_orm::MAX_WASM_IMPLS_PER_BUILD)
        .flat_map(|i| {
            let mut one = stand_ins(&[("shout", ImplKind::Define)], &log);
            one[0].method = format!("cap_probe_{i}");
            one
        })
        .collect();
    let err = with_wasm(&h, many).err().expect("over the cap");
    assert_eq!(
        err.kind(),
        erp_fields::ErrorKind::ResourceExhausted,
        "{err}"
    );
    assert!(!erp_orm::wasm_method_name_is_interned("cap_probe_0"));
    // later builds still work
    with_wasm(&h, stand_ins(&[("shout", ImplKind::Define)], &log)).expect("a normal build");
}
