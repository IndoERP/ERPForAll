//! A real WebAssembly component in the override chains (
//! -014, -015): `test_wasm_dual_b` built for `wasm32-wasip2` runs between the native `test_wasm_dual_a`
//! and `test_wasm_dual_c`, through `erp-wasm-host` and the ORM's access-checked entry points.
//!
//! The tests are `#[ignore]`d in the workspace run and fail, never skip, when they run without the
//! artefact: `scripts/ci.sh` step `wasm-dispatch` builds the component (fat LTO) and sets
//! `ERP_WASM_DUAL_COMPONENT`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use erp_addon_test_wasm_dual_a::{ADDON as ADDON_A, MARK as MARK_A};
use erp_addon_test_wasm_dual_b::{ADDON as ADDON_B, MARK as MARK_B};
use erp_addon_test_wasm_dual_c::{ADDON as ADDON_C, MARK as MARK_C};
use erp_fields::ErrorKind;
use erp_orm::{
    BoxFuture, Context, DynSuper, ImplKind, MethodInvoker, Orm, Records, Transaction,
    WasmMethodImpl, call_method_json,
};
use erp_sdk::Value;
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_wasm_host::{
    CallableMethods, InstancePools, Limits, OrmCapabilities, OrmOp, WasmAddon, WasmHost,
    callable_methods,
};

const MODEL: &str = "test.wasm.dual";

struct World {
    h: Harness,
    orm: Orm,
    addon: Arc<WasmAddon>,
}

fn component_bytes() -> Vec<u8> {
    let path = std::env::var("ERP_WASM_DUAL_COMPONENT")
        .unwrap_or_else(|_| panic!("ERP_WASM_DUAL_COMPONENT must name the dual-b component"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

/// A stand-in for the native layer C of `shout`, written in the test so a host request can be made
/// slow or large on demand.
#[derive(Debug)]
struct CStandIn {
    delay: Duration,
    /// Repeats the answer this many times (1 leaves it as it is).
    repeat: usize,
}

impl MethodInvoker for CStandIn {
    fn addon(&self) -> &str {
        ADDON_C.name
    }

    fn kind(&self) -> ImplKind {
        ImplKind::Override
    }

    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, erp_fields::Result<String>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            let inner = sup.call_json(tx, records, args_json).await?;
            let text: String = serde_json::from_str(&inner).unwrap();
            Ok(json_str(&text.repeat(self.repeat)))
        })
    }
}

async fn world(with_c: bool, caps: OrmCapabilities, limits: Limits) -> World {
    world_with(with_c, caps, limits, None).await
}

/// [`world`] with the layer C of `shout` replaced by `c_stand_in` (needs `with_c`).
async fn world_with(
    with_c: bool,
    caps: OrmCapabilities,
    limits: Limits,
    c_stand_in: Option<CStandIn>,
) -> World {
    world_custom(with_c, caps, limits, c_stand_in, WorldSchema::default()).await
}

#[derive(Default)]
struct WorldSchema {
    decls: Vec<erp_registry::ModelDecl>,
    policy: Option<erp_testkit::FieldPolicy>,
    addons: Vec<&'static erp_registry::AddonDescriptor>,
}

async fn world_custom(
    with_c: bool,
    caps: OrmCapabilities,
    limits: Limits,
    c_stand_in: Option<CStandIn>,
    schema: WorldSchema,
) -> World {
    let mut addons = vec![&base_users::ADDON, &ADDON_A, &ADDON_B];
    if with_c {
        addons.push(&ADDON_C);
    }
    // the transaction's own identity is an ordinary user: in a System or elevated environment a
    // guest may only continue its chain (security review r1 H1)
    addons.extend(schema.addons);
    let builder = Harness::builder(addons).as_user(4).with_decls(schema.decls);
    let builder = match schema.policy {
        Some(policy) => builder.policy(policy),
        None => builder,
    };
    let h = builder.build().await.unwrap();
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
    let component = host.component(&component_bytes()).unwrap();
    // describe first (it names the methods), then the real addon with its callable list
    let probe = WasmAddon::new(
        Arc::clone(&host),
        ADDON_B.name,
        component.clone(),
        limits,
        caps,
        CallableMethods::default(),
    );
    let describe = probe.describe().await.unwrap();
    let callable = callable_methods(h.orm(), ADDON_B.name, &describe).unwrap();
    let addon = WasmAddon::new(host, ADDON_B.name, component, limits, caps, callable);
    let mut wasm = addon.method_impls(&describe).unwrap();
    if let Some(stand_in) = c_stand_in {
        wasm.push(WasmMethodImpl {
            model: MODEL.to_owned(),
            method: "shout".to_owned(),
            invoker: Arc::new(stand_in),
        });
    }
    let orm = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(wasm)
        .build()
        .unwrap();
    World { h, orm, addon }
}

fn caps_all() -> OrmCapabilities {
    OrmCapabilities::NONE
        .with(OrmOp::Read)
        .with(OrmOp::Write)
        .with(OrmOp::CallMethod)
}

async fn begin(w: &World) -> Transaction {
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    w.orm.begin_with(w.h.principal(), ctx).await.unwrap()
}

async fn call(
    tx: &mut Transaction,
    ids: &[i64],
    method: &str,
    args: &str,
) -> erp_fields::Result<String> {
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(ids.iter().copied())?;
    call_method_json(tx, &records, method, args).await
}

fn json_str(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn native_over_a_component_over_native() {
    let w = world(true, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    let got = call(&mut tx, &[], "label", r#"["x"]"#).await.unwrap();
    assert_eq!(got, json_str(&format!("x{MARK_A}{MARK_B}{MARK_C}")));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_component_over_native() {
    let w = world(false, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    let got = call(&mut tx, &[], "label", r#"["x"]"#).await.unwrap();
    assert_eq!(got, json_str(&format!("x{MARK_A}{MARK_B}")));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn native_over_a_component_definition() {
    let w = world(true, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    let got = call(&mut tx, &[], "shout", r#"["hello"]"#).await.unwrap();
    assert_eq!(got, json_str(&format!("HELLO{MARK_C}")));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn re_entry_leases_a_second_instance() {
    let w = world(true, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    // nested -> call-method shout -> native C -> component shout: the addon is entered twice
    let got = call(&mut tx, &[], "nested", r#"["hello"]"#).await.unwrap();
    assert_eq!(got, json_str(&format!("[HELLO{MARK_C}]")));
    let pools = tx.extension::<InstancePools>();
    assert_eq!(pools.created(), 2, "one instance per level of re-entry");
    assert_eq!(pools.idle(ADDON_B.name), 2, "both are back in the pool");
    // a second call reuses them
    call(&mut tx, &[], "nested", r#"["again"]"#).await.unwrap();
    assert_eq!(tx.extension::<InstancePools>().created(), 2);
    tx.rollback().await.unwrap();
}

async fn call_in_zone(
    tx: &mut Transaction,
    tz: Option<&str>,
    method: &str,
    args: &str,
) -> erp_fields::Result<String> {
    let env = match tz {
        Some(tz) => tx.env().with_context([("tz", Value::str(tz))]),
        None => tx.env(),
    };
    let records = env.records(MODEL).unwrap();
    call_method_json(tx, &records, method, args).await
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn the_guest_reads_today_in_the_zone_of_the_call() {
    use erp_fields::Date;
    let w = world(false, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    // +14h and -11h: at every instant one of them is on another date than UTC
    for tz in [
        None,
        Some("UTC"),
        Some("Pacific/Kiritimati"),
        Some("Pacific/Pago_Pago"),
    ] {
        let zone = tz.unwrap_or("UTC");
        let before = Date::context_today(zone).unwrap().days();
        let got = call_in_zone(&mut tx, tz, "today", "null").await.unwrap();
        let after = Date::context_today(zone).unwrap().days();
        let got: i32 = got.parse().unwrap();
        assert!(
            (before..=after).contains(&got),
            "{zone}: {got} not in {before}..={after}"
        );
    }
    tx.rollback().await.unwrap();
}

async fn make_record(tx: &mut Transaction, qty: i64) -> i64 {
    let records = tx.env().records(MODEL).unwrap();
    let mut row = BTreeMap::new();
    row.insert("name".to_owned(), Value::Str("n".into()));
    row.insert("qty".to_owned(), Value::Int(qty));
    records.create_values(tx, &[row]).await.unwrap().ids()[0]
}

async fn qty_of(tx: &mut Transaction, id: i64) -> Value {
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    records
        .read(tx, &["qty"])
        .await
        .unwrap()
        .remove(0)
        .remove("qty")
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn the_guest_reads_and_writes_through_the_orm() {
    let w = world(false, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    let id = make_record(&mut tx, 3).await;
    let got = call(&mut tx, &[id], "double_qty_now", "null")
        .await
        .unwrap();
    assert_eq!(got, "3");
    assert_eq!(qty_of(&mut tx, id).await, Value::Int(6));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn an_unapproved_write_is_refused() {
    let caps = OrmCapabilities::NONE.with(OrmOp::Read);
    let w = world(false, caps, Limits::default()).await;
    let mut tx = begin(&w).await;
    let id = make_record(&mut tx, 3).await;
    let err = call(&mut tx, &[id], "double_qty_now", "null")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(err.message().contains("orm:write"), "{}", err.message());
    assert_eq!(qty_of(&mut tx, id).await, Value::Int(3));
    // an ordinary refusal by the guest's own request: the transaction stays usable
    assert!(!tx.is_poisoned());
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_runaway_guest_is_stopped_and_poisons_the_transaction() {
    let limits = Limits::default()
        .with_cpu(Duration::from_millis(100))
        .unwrap();
    let w = world(false, caps_all(), limits).await;
    let mut tx = begin(&w).await;
    let started = std::time::Instant::now();
    let err = call(&mut tx, &[], "spin", "null").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopped within the deadline"
    );
    assert!(tx.is_poisoned(), "a commit must refuse");
    assert_eq!(
        tx.extension::<InstancePools>().idle(w.addon.name()),
        0,
        "the trapped instance is not reused"
    );
    tx.rollback().await.unwrap();
    // the host and the addon are fine for a new transaction
    let mut tx = begin(&w).await;
    let got = call(&mut tx, &[], "shout", r#"["ok"]"#).await.unwrap();
    assert_eq!(got, json_str("OK"));
    tx.rollback().await.unwrap();
}

/// and the id of the request it ran for, and the request trace names the addon for the RPC layer.
#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_stopped_guest_logs_its_addon_method_and_request_id() {
    use erp_server::logging::{LineLogger, LogFilter};
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let limits = Limits::default()
        .with_cpu(Duration::from_millis(100))
        .unwrap();
    let w = world(false, caps_all(), limits).await;
    let sink = Sink::default();
    let _guard = tracing::subscriber::set_default(LineLogger::new(
        LogFilter::parse("warn").unwrap(),
        sink.clone(),
    ));
    let id = "0194f5a0-7b1c-7d2e-8f3a-123456789abc";
    let trace = erp_orm::trace::RequestTrace::new(Some(id));
    let mut tx = begin(&w).await;
    let err =
        erp_orm::trace::with_request_trace(Arc::clone(&trace), call(&mut tx, &[], "spin", "null"))
            .await
            .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
    tx.rollback().await.unwrap();
    assert_eq!(trace.addon().as_deref(), Some(w.addon.name()));
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("WebAssembly addon call failed"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    for part in [
        " WARN  erp::wasm_host:".to_owned(),
        format!("addon={}", w.addon.name()),
        format!("entry={MODEL}.spin"),
        format!("request_id={id}"),
        "kind=ResourceExhausted".to_owned(),
        "abort=true".to_owned(),
    ] {
        assert!(lines[0].contains(&part), "{part} missing in {}", lines[0]);
    }
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_guest_cannot_call_a_hidden_method_by_name() {
    let w = world(false, caps_all(), Limits::default()).await;
    let mut tx = begin(&w).await;
    let id = make_record(&mut tx, 3).await;
    let err = call(&mut tx, &[id], "call_hidden", "null")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(err.message().contains("hidden_probe"), "{}", err.message());
    assert!(!tx.is_poisoned(), "an ordinary refusal");
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn the_memory_of_a_transactions_stores_is_bounded() {
    // room for one store at the default 32 MiB cap, not for two
    let limits = Limits::default()
        .with_tx_memory_bytes(40 * 1024 * 1024)
        .unwrap();
    let w = world(true, caps_all(), limits).await;
    let mut tx = begin(&w).await;
    let err = call(&mut tx, &[], "nested", r#"["hello"]"#)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
    assert!(
        err.message().contains("bytes of guest memory"),
        "{}",
        err.message()
    );
    assert_eq!(tx.extension::<InstancePools>().created(), 1);
    // a call that needs one store still works
    let got = call(&mut tx, &[], "shout", r#"["ok"]"#).await.unwrap();
    assert_eq!(got, json_str(&format!("OK{MARK_C}")));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn idle_stores_beyond_the_bound_are_dropped() {
    let limits = Limits::default().with_idle_stores(1).unwrap();
    let w = world(true, caps_all(), limits).await;
    let mut tx = begin(&w).await;
    call(&mut tx, &[], "nested", r#"["hello"]"#).await.unwrap();
    let pools = tx.extension::<InstancePools>();
    assert_eq!(pools.created(), 2, "the re-entry needed a second store");
    assert_eq!(pools.idle(ADDON_B.name), 1, "only one idle store is kept");
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_slow_backend_request_hits_the_wall_clock_budget() {
    let limits = Limits::default()
        .with_cpu(Duration::from_millis(150))
        .unwrap();
    let slow = CStandIn {
        delay: Duration::from_secs(3),
        repeat: 1,
    };
    let w = world_with(true, caps_all(), limits, Some(slow)).await;
    let mut tx = begin(&w).await;
    let started = std::time::Instant::now();
    let err = call(&mut tx, &[], "nested", r#"["hello"]"#)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
    assert!(err.message().contains("time budget"), "{}", err.message());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stopped by the budget, not by the slow request: {:?}",
        started.elapsed()
    );
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn a_failed_check_after_a_write_cannot_commit() {
    let limits = Limits::default().with_payload_bytes(64).unwrap();
    let big = CStandIn {
        delay: Duration::ZERO,
        repeat: 4,
    };
    let w = world_with(true, caps_all(), limits, Some(big)).await;
    let mut tx = begin(&w).await;
    let id = make_record(&mut tx, 3).await;
    // 20 characters: the arguments fit the limit, the quadrupled answer does not
    let text = json_str("abcdefghijklmnopqrst");
    let err = call(&mut tx, &[id], "write_then_shout", &format!("[{text}]"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted, "{err}");
    assert!(err.message().contains("payload bytes"), "{}", err.message());
    assert!(
        tx.is_poisoned(),
        "the write already reached the transaction"
    );
    assert!(
        tx.commit().await.is_err(),
        "a poisoned transaction must not commit"
    );
    // nothing of the transaction persisted
    let mut tx = begin(&w).await;
    let found = tx
        .env()
        .records(MODEL)
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(found.len(), 0);
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
async fn temporal_cells_cross_a_real_component() {
    let w = world(
        false,
        caps_all().with(OrmOp::Create).with(OrmOp::Unlink),
        Limits::default(),
    )
    .await;
    let ctx = Context::from_pairs([
        ("allowed_company_ids", Value::list([Value::Int(1)])),
        ("tz", Value::str("Asia/Jakarta")),
    ]);
    let mut tx = w.orm.begin_with(w.h.principal(), ctx).await.unwrap();
    exercise_temporal_methods(&mut tx).await;
    exercise_temporal_json(&mut tx, true).await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn temporal_cells_use_the_native_backend() {
    let h = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_B])
        .build()
        .await
        .unwrap();
    let mut tx = h.orm().begin(h.principal()).await.unwrap();
    exercise_temporal_methods(&mut tx).await;
    exercise_temporal_json(&mut tx, false).await;
    tx.rollback().await.unwrap();
}

async fn exercise_temporal_methods(tx: &mut Transaction) {
    use erp_fields::{Date, DateTime};
    let id = make_record(tx, 1).await;
    let day = Date::from_ymd(2024, 2, 29).unwrap();
    let datetime = DateTime::from_parts(day, 23, 59, 58, 123456).unwrap();
    let args = serde_json::to_string(&(Some(day.days()), Some(datetime.micros()))).unwrap();
    let before = call(tx, &[id], "temporal_swap", &args).await.unwrap();
    assert_eq!(
        serde_json::from_str::<(Option<i32>, Option<i64>)>(&before).unwrap(),
        (None, None)
    );
    let mapped = call(tx, &[id], "temporal_mapped", "null").await.unwrap();
    assert_eq!(
        serde_json::from_str::<(Vec<Option<i32>>, Vec<Option<i64>>)>(&mapped).unwrap(),
        (vec![Some(day.days())], vec![Some(datetime.micros())])
    );
    let second = make_record(tx, 2).await;
    let mapped = call(tx, &[second, id], "temporal_mapped", "null")
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<(Vec<Option<i32>>, Vec<Option<i64>>)>(&mapped).unwrap(),
        (
            vec![None, Some(day.days())],
            vec![None, Some(datetime.micros())]
        )
    );
    let before = call(tx, &[id], "temporal_swap", "[-1,-1]").await.unwrap();
    assert_eq!(
        serde_json::from_str::<(Option<i32>, Option<i64>)>(&before).unwrap(),
        (Some(day.days()), Some(datetime.micros()))
    );
    let before = call(tx, &[id], "temporal_swap", "[null,null]")
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<(Option<i32>, Option<i64>)>(&before).unwrap(),
        (Some(-1), Some(-1))
    );
    let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let values = records
        .read(tx, &["loan_date", "returned_at"])
        .await
        .unwrap();
    assert_eq!(values[0]["loan_date"], Value::False);
    assert_eq!(values[0]["returned_at"], Value::False);
    let empty = call(tx, &[], "temporal_mapped", "null").await.unwrap();
    assert_eq!(
        serde_json::from_str::<(Vec<Option<i32>>, Vec<Option<i64>>)>(&empty).unwrap(),
        (vec![], vec![])
    );
}

/// JSON lifecycle overrides use server-text precision; field-aware native validation stays active.
async fn exercise_temporal_json(tx: &mut Transaction, component: bool) {
    use erp_fields::{Date, DateTime};
    let date = Date::from_ymd(2024, 2, 29).unwrap();
    let datetime = DateTime::from_parts(date, 12, 34, 56, 123456).unwrap();
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let made = model
        .create_values(
            tx,
            &[BTreeMap::from([
                ("loan_date".into(), Value::Date(date)),
                ("returned_at".into(), Value::DateTime(datetime)),
            ])],
        )
        .await
        .unwrap();
    let row = made.read(tx, &["loan_date", "returned_at"]).await.unwrap();
    assert_eq!(row[0]["loan_date"], Value::Date(date));
    assert_eq!(
        row[0]["returned_at"],
        Value::DateTime(
            DateTime::from_parts(date, 12, 34, 56, if component { 0 } else { 123456 }).unwrap()
        )
    );
    let updated = DateTime::from_parts(date, 1, 2, 3, 654321).unwrap();
    made.write_values(
        tx,
        &BTreeMap::from([("returned_at".into(), Value::DateTime(updated))]),
    )
    .await
    .unwrap();
    let row = made.read(tx, &["returned_at"]).await.unwrap();
    assert_eq!(
        row[0]["returned_at"],
        Value::DateTime(
            DateTime::from_parts(date, 1, 2, 3, if component { 0 } else { 654321 }).unwrap()
        )
    );
    made.write_values(
        tx,
        &BTreeMap::from([
            ("loan_date".into(), Value::str("1969-12-31")),
            ("returned_at".into(), Value::str("1969-12-31 23:59:59")),
        ]),
    )
    .await
    .unwrap();
    let row = made.read(tx, &["loan_date", "returned_at"]).await.unwrap();
    assert_eq!(row[0]["loan_date"], Value::Date(Date::from_days(-1)));
    assert_eq!(
        row[0]["returned_at"],
        Value::DateTime(DateTime::from_micros(-1_000_000))
    );
    for (field, text) in [
        ("loan_date", "2023-02-29"),
        ("returned_at", "2024-02-29T12:34:56Z"),
    ] {
        let err = made
            .write_values(tx, &BTreeMap::from([(field.into(), Value::str(text))]))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for test.wasm.temporal.{field}: '{text}'")
        );
    }
}

static NATIVE_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "test_outbox_native",
    depends: &["test_wasm_dual_b"],
    exact_only: false,
};

fn native_schema(policy: Option<erp_testkit::FieldPolicy>) -> WorldSchema {
    use erp_registry::{FieldDecl, ModelDecl};
    WorldSchema {
        decls: vec![
            ModelDecl::new(NATIVE_ADDON.name, "test.wasm.temporal")
                .inherit(&["test.wasm.temporal"])
                .fields(
                    const {
                        &[
                            FieldDecl::uuid("native_uuid"),
                            FieldDecl::xid8("native_xid"),
                            FieldDecl::timestamptz("native_time"),
                            FieldDecl::many2many("native_child_ids", "test.wasm.native.child"),
                        ]
                    },
                ),
            ModelDecl::new(NATIVE_ADDON.name, "test.wasm.native.child").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::uuid("native_uuid"),
                        FieldDecl::xid8("native_xid"),
                        FieldDecl::timestamptz("native_time"),
                        FieldDecl::many2many("nested_ids", "test.wasm.native.child").relation(
                            "test_wasm_native_child_nested_rel",
                            "source_id",
                            "target_id",
                        ),
                    ]
                },
            ),
        ],
        policy,
        addons: vec![&NATIVE_ADDON],
    }
}

async fn native_world(policy: Option<erp_testkit::FieldPolicy>) -> World {
    world_custom(
        false,
        caps_all().with(OrmOp::Create).with(OrmOp::Unlink),
        Limits::default(),
        None,
        native_schema(policy),
    )
    .await
}

async fn native_probe(
    tx: &mut Transaction,
    ids: &[i64],
    operation: &str,
    field: &str,
    payload: &str,
) -> String {
    let model = tx
        .env()
        .records("test.wasm.temporal")
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    let args = serde_json::to_string(&(operation, field, payload)).unwrap();
    let answer = call_method_json(tx, &model, "abi1_native_probe", &args)
        .await
        .unwrap();
    serde_json::from_str(&answer).unwrap()
}

#[tokio::test]
#[ignore = "requires a real ABI1 component; old-source artifact provenance belongs to the runner"]
async fn native_fields_legacy_neighbor_component_stays_active() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    exercise_temporal_json(&mut tx, true).await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_guest_host_requests_refuse_per_field_before_mutation() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let made = model
        .create_values(&mut tx, &[BTreeMap::new()])
        .await
        .unwrap();
    let id = made.ids()[0];
    for field in ["native_uuid", "native_xid", "native_time"] {
        for ids in [vec![], vec![id]] {
            for op in ["read", "write", "columns", "create"] {
                let before = model
                    .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                    .await
                    .unwrap();
                let result = native_probe(&mut tx, &ids, op, field, "null").await;
                assert!(
                    result.starts_with("BoundaryErrorKind::Unsupported:"),
                    "{op} {field}: {result}"
                );
                assert!(result.contains("future ABI major"), "{result}");
                assert_eq!(
                    model
                        .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                        .await
                        .unwrap(),
                    before
                );
            }
        }
    }
    let ordinary = native_probe(&mut tx, &[id], "read", "loan_date", "null").await;
    assert_eq!(ordinary, "ok");
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_generic_json_and_nested_commands_refuse_atomically() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let model = tx.env().records("test.wasm.temporal").unwrap();
    for payload in [
        serde_json::json!([[{"native_uuid": "00000000-0000-0000-0000-000000000000"}]]),
        serde_json::json!([[{}, {"native_xid": null}]]),
        serde_json::json!([[{}, {"native_child_ids": [[0, 0, {"native_uuid": null}]]}]]),
    ] {
        let before = model
            .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap();
        let result = native_probe(&mut tx, &[], "call-create", "", &payload.to_string()).await;
        assert!(
            result.starts_with("BoundaryErrorKind::Unsupported:"),
            "{result}"
        );
        assert!(result.contains("future ABI major"), "{result}");
        assert_eq!(
            model
                .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                .await
                .unwrap(),
            before
        );
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_acl_precedes_unsupported_even_for_empty_requests() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny("test.wasm.temporal", "native_uuid", erp_orm::FieldOp::Read)
        .deny("test.wasm.temporal", "native_uuid", erp_orm::FieldOp::Write);
    let w = native_world(Some(policy)).await;
    let mut tx = begin(&w).await;
    for op in ["read", "write", "columns", "create"] {
        let result = native_probe(&mut tx, &[], op, "native_uuid", "null").await;
        assert!(
            result.starts_with("BoundaryErrorKind::AccessDenied:"),
            "{op}: {result}"
        );
        assert!(!result.contains("future ABI major"), "{result}");
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_nested_update_and_replace_keep_child_values_unchanged() {
    use erp_fields::Uuid;
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let child_model = tx.env().records("test.wasm.native.child").unwrap();
    let child = child_model
        .create_values(
            &mut tx,
            &[BTreeMap::from([
                ("name".into(), Value::str("child")),
                ("native_uuid".into(), Value::Uuid(Uuid::NIL)),
            ])],
        )
        .await
        .unwrap();
    let child_id = child.ids()[0];
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let root = model
        .create_values(&mut tx, &[BTreeMap::new()])
        .await
        .unwrap();
    let root_id = root.ids()[0];
    let set = serde_json::json!([{ "native_child_ids": [[6, 0, [child_id]]] }]);
    assert_eq!(
        native_probe(&mut tx, &[root_id], "call-write", "", &set.to_string()).await,
        "ok"
    );
    let before: Option<Uuid> = tx
        .query(
            "SELECT native_uuid FROM test_wasm_native_child WHERE id = $1",
            &[&child_id],
        )
        .await
        .unwrap()[0]
        .try_get(0)
        .unwrap();
    let update =
        serde_json::json!([{ "native_child_ids": [[1, child_id, {"native_uuid": null}]] }]);
    let result = native_probe(&mut tx, &[root_id], "call-write", "", &update.to_string()).await;
    assert!(
        result.starts_with("BoundaryErrorKind::Unsupported:"),
        "{result}"
    );
    let after: Option<Uuid> = tx
        .query(
            "SELECT native_uuid FROM test_wasm_native_child WHERE id = $1",
            &[&child_id],
        )
        .await
        .unwrap()[0]
        .try_get(0)
        .unwrap();
    assert_eq!(after, before);
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_unknown_field_diagnostics_do_not_preempt_denial() {
    let policy = erp_testkit::FieldPolicy::new().deny(
        "test.wasm.temporal",
        "native_uuid",
        erp_orm::FieldOp::Write,
    );
    let w = native_world(Some(policy)).await;
    let mut tx = begin(&w).await;
    let mixed = serde_json::json!([[{"native_uuid": null, "zzz_missing": null}]]);
    let result = native_probe(&mut tx, &[], "call-create", "", &mixed.to_string()).await;
    assert!(
        result.starts_with("BoundaryErrorKind::AccessDenied:"),
        "{result}"
    );
    assert!(!result.contains("zzz_missing"), "{result}");
    tx.rollback().await.unwrap();

    let w = native_world(Some(
        erp_testkit::FieldPolicy::new().deny_model("test.wasm.temporal"),
    ))
    .await;
    let mut tx = begin(&w).await;
    let result = native_probe(&mut tx, &[], "read", "zzz_missing", "null").await;
    assert!(
        result.starts_with("BoundaryErrorKind::AccessDenied:"),
        "{result}"
    );
    assert!(!result.contains("zzz_missing"), "{result}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_context_does_not_silently_stringify_tagged_scalars() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let tagged = model.with_context([(
        "tagged_native",
        Value::list([Value::Xid8(erp_fields::Xid8::from_u64(u64::MAX))]),
    )]);
    let args = serde_json::to_string(&("read", "loan_date", "null")).unwrap();
    let error = call_method_json(&mut tx, &tagged, "abi1_native_probe", &args)
        .await
        .unwrap_err();
    assert!(error.message().contains("native xid8"), "{error}");
    assert!(error.message().contains("future ABI major"), "{error}");
    let opaque = model.with_context([(
        "opaque_json",
        Value::Json(Arc::new(
            serde_json::json!({"xid8": "18446744073709551615"}),
        )),
    )]);
    let answer = call_method_json(&mut tx, &opaque, "abi1_native_probe", &args)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<String>(&answer).unwrap(), "ok");
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_guest_super_injection_cannot_bypass_field_admission() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let made = model
        .create_values(&mut tx, &[BTreeMap::new()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = model
        .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
        .await
        .unwrap();
    let injected = made.with_context([("abi1_inject_native_on_super", Value::Bool(true))]);
    for (operation, payload) in [
        (
            "call-create",
            serde_json::json!([[{"loan_date": "2026-11-01"}]]),
        ),
        (
            "call-write",
            serde_json::json!([{"loan_date": "2026-11-01"}]),
        ),
    ] {
        let args = serde_json::to_string(&(operation, "", payload.to_string())).unwrap();
        let answer = call_method_json(&mut tx, &injected, "abi1_native_probe", &args)
            .await
            .unwrap();
        let answer: String = serde_json::from_str(&answer).unwrap();
        assert_ne!(answer, "ok", "guest-added native NULL must fail at Super");
        assert!(answer.contains("future ABI major"), "{answer}");
        assert_eq!(
            model
                .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                .await
                .unwrap(),
            before
        );
        let row = tx
            .query(
                "SELECT loan_date::text FROM test_wasm_temporal ORDER BY id",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].get::<_, Option<String>>(0), None);
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn native_fields_preencode_admission_bounds_recursive_and_mixed_batches() {
    use erp_sdk::{Cell, Command, RawVals};
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let model = tx.env().records("test.wasm.temporal").unwrap();
    let mut nested = RawVals::new().set_cell("native_uuid", Cell::Null);
    for _ in 0..64 {
        nested = RawVals::new().set_commands("nested_ids", [Command::Create(nested)]);
    }
    let root = RawVals::new().set_commands("native_child_ids", [Command::Create(nested)]);
    for batch in [vec![RawVals::new(), root], vec![RawVals::new(); 100_001]] {
        let error = model.create(&mut tx, batch).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
        assert!(error.message().contains("admission limit"), "{error}");
        assert_eq!(
            model
                .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                .await
                .unwrap(),
            0
        );
        let child = tx.env().records("test.wasm.native.child").unwrap();
        assert_eq!(
            child
                .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                .await
                .unwrap(),
            0
        );
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the ABI1 probe component"]
async fn guest_native_only_chains_refuse_native_values_before_mutation() {
    let w = native_world(None).await;
    let mut tx = begin(&w).await;
    let child = tx.env().records("test.wasm.native.child").unwrap();
    let made = child
        .create_values(
            &mut tx,
            &[BTreeMap::from([("name".into(), Value::str("original"))])],
        )
        .await
        .unwrap();
    let id = made.ids()[0];
    let probe = tx.env().records("test.wasm.temporal").unwrap();
    for (field, value) in [
        (
            "native_uuid",
            serde_json::json!("00000000-0000-0000-0000-000000000001"),
        ),
        ("native_xid", serde_json::json!("18446744073709551615")),
        ("native_time", serde_json::json!("2026-11-05T00:00:00Z")),
    ] {
        for value in [value, serde_json::Value::Null] {
            for method in ["create", "write"] {
                let map = serde_json::json!({field: value.clone()});
                let payload = if method == "create" {
                    serde_json::json!([[{"name":"lost"}, map]])
                } else {
                    serde_json::json!([map])
                };
                let args = serde_json::to_string(&(method, vec![id], payload.to_string())).unwrap();
                let result = call_method_json(&mut tx, &probe, "abi1_native_child_probe", &args)
                    .await
                    .unwrap();
                let result: String = serde_json::from_str(&result).unwrap();
                let kind = match field {
                    "native_uuid" => "uuid",
                    "native_xid" => "xid8",
                    "native_time" => "timestamptz",
                    _ => unreachable!("fixed test cases"),
                };
                assert_eq!(
                    result,
                    format!(
                        "BoundaryErrorKind::Unsupported:field 'test.wasm.native.child.{field}' ({kind}) is not supported across frozen WASM ABI1; requires a future ABI major with typed native fields"
                    )
                );
            }
        }
    }
    for payload in [
        serde_json::json!([[{"nested_ids": [[0,0,{"native_uuid":null}]]}]]),
        serde_json::json!([[{"nested_ids": [[1,id,{"native_xid":null}]]}]]),
    ] {
        let args =
            serde_json::to_string(&("create", Vec::<i64>::new(), payload.to_string())).unwrap();
        let result = call_method_json(&mut tx, &probe, "abi1_native_child_probe", &args)
            .await
            .unwrap();
        let result: String = serde_json::from_str(&result).unwrap();
        assert!(
            result.starts_with("BoundaryErrorKind::Unsupported:"),
            "{result}"
        );
    }
    assert_eq!(
        child
            .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        made.value(&mut tx, "name").await.unwrap(),
        Value::str("original")
    );
    let args = serde_json::to_string(&(
        "create",
        Vec::<i64>::new(),
        "[[{\"name\":\"ordinary1\"},{\"name\":\"ordinary2\"}]]",
    ))
    .unwrap();
    let result = call_method_json(&mut tx, &probe, "abi1_native_child_probe", &args)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<String>(&result).unwrap(), "ok");
    // Host-origin native-only JSON continues to support native fields.
    call_method_json(
        &mut tx,
        &child,
        "create",
        "[[{\"native_xid\":\"18446744073709551615\"}]]",
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}
