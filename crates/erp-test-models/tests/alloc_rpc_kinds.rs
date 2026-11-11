//! Allocations per JSON-2 request of today's served path (PERF.AUDIT-SERVE,
//! ; G6 item 5 and its checkpoint): `RpcService::call`
//! for each BENCH-007 kind (form, list, name_search, save, create) and the rejection, on the
//! bench models, with the request bodies of BENCH-007, whole process (dhat).
//!
//! The form read and the list page are also split into the layers budgets separately:
//! the transaction lifecycle (begin, opening flight, `COMMIT`), one method statement on a warm
//! connection, the ORM method called directly, and the result writer. The printed "whole − layers"
//! line is a residual, not the protocol layer. Heap profiles of both paths (perf-audit-serve.md §1)
//! show what else it holds: two allocations of the service that `direct()` does not make in its
//! windows (the read-only entry `Arc<str>` built per attempt, which `direct()` prepares outside,
//! and the per-call unit-of-work flag), less what only `direct()` allocates (its `Value::list` on
//! the form read, two cache columns, and `block_on` boxing two of its layer futures where the
//! service's call is boxed once: 3 to 4 blocks). The opening flight costs the same on both paths.
//! So the audit measures the protocol layer from the heap profile.
//!
//! **Whole process.** The counting allocator (dhat) counts every thread, so the tests run on the
//! main thread under `erp_alloc_budget::harness` (`harness = false`, lesson): one at a time,
//! and no test-runner thread allocates inside a window.
//!
//! Database-backed figures are means over [`MEASURED`] requests after [`WARM`] warm-up requests, each
//! request (or layer) measured on its own (`erp_alloc_budget::measure` per request, summed), on a
//! current-thread runtime with one pooled connection in use. The ceilings are today's figures
//! after subtracting exact delivered-plus-parked driver blocks. Future optimization lowers the
//! ceiling in the commit that lowers the cost. Two more tests pin the list page's cost per added order row and the
//! round trips per kind. The deterministic rejection includes both warm-up and measured
//! requests in its mean.
//!
//! Policy and principal as the in-process BENCH producer and the floor's capture
//! (`StaticPolicy::AllowAll`, uid 2, no signal): access rules and the session check are
//! not in these figures.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "tests"
)]

mod bench_fixture;

use std::sync::Arc;

use erp_fields::Value;
use erp_orm::ResponseBatches;
use erp_orm::{BeginOptions, Context, Principal, PrincipalBuilder, Spec, SpecItem, WebSearchRead};
use erp_rpc::{Caller, InfraError, RpcCall, RpcConfig, RpcFailure, RpcHost, RpcService};
use erp_sdk::{Domain, UserId};
use erp_test_models::{base_users, bench};
use erp_testkit::Harness;
use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::DriverBlocks;
use serde_json::json;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    rpc_kinds_allocate_within_todays_ceilings,
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    rpc_list_page_cost_per_row,
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    rpc_kinds_round_trips,
    rpc_kinds_semantics,
);

/// Unmeasured requests first: statements prepared, pools and caches filled.
const WARM: usize = 64;
/// Measured requests per figure.
const MEASURED: usize = 200;
/// Orders in the dataset (10 lines each, 2 tags each), all `confirmed`.
const ORDERS: usize = 300;

/// Blocks and requested bytes summed over measured requests.
#[derive(Debug, Default, Clone, Copy)]
struct Sum {
    blocks: u64,
    bytes: u64,
    n: u64,
    min: f64,
    max: f64,
    driver_blocks: f64,
    delivered: u64,
    parked: u64,
}

impl Sum {
    fn add(&mut self, s: &AllocStats, driver: DriverBlocks) {
        let net = s.total_blocks as f64 - driver.blocks();
        assert!(net >= 0.0, "driver blocks belong to this allocation window");
        if self.n == 0 || net < self.min {
            self.min = net;
        }
        self.max = self.max.max(net);
        self.blocks += s.total_blocks;
        self.bytes += s.total_bytes;
        self.n += 1;
        self.driver_blocks += driver.blocks();
        self.delivered += driver.delivered();
        self.parked += driver.parked();
    }

    fn blocks(&self) -> f64 {
        (self.blocks as f64 - self.driver_blocks) / self.n as f64
    }

    fn bytes(&self) -> f64 {
        self.bytes as f64 / self.n as f64
    }

    fn line(&self, what: &str) -> String {
        format!(
            "{what}: {:.2} net blocks, {:.0} raw B per request (min {} max {} net blocks, n {}; raw {} blocks, {} delivered, {} parked)",
            self.blocks(),
            self.bytes(),
            self.min,
            self.max,
            self.n,
            self.blocks,
            self.delivered,
            self.parked,
        )
    }
}

/// Measure `work` once and add it to `sum`.
fn measured<R>(sum: &mut Sum, rt: &tokio::runtime::Runtime, work: impl FnOnce() -> R) -> R {
    let before = ResponseBatches::process_wide();
    let (stats, out) = alloc::measure(work).expect("measure");
    let window = ResponseBatches::process_wide()
        .since(before)
        .expect("process batch counters must grow within one scope");
    let driver = DriverBlocks::new(rt.handle(), window).expect("current-thread batch accounting");
    sum.add(&stats, driver);
    out
}

struct Host;

impl RpcHost for Host {
    fn language_active(&self, _code: &str) -> erp_fields::Result<bool> {
        Ok(false)
    }

    fn has_capability(
        &self,
        _principal: &Principal,
        _capability: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        Ok(false)
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn principal() -> Principal {
    PrincipalBuilder::user(UserId::new(2).unwrap())
        .build()
        .unwrap()
}

/// The BENCH-007 context (`erp-bench` `serve::request::context`).
fn context_json() -> serde_json::Value {
    json!({"tz": "UTC", "lang": "en_US"})
}

fn context() -> Context {
    Context::from_pairs([("tz", Value::str("UTC")), ("lang", Value::str("en_US"))])
}

/// The bench dataset, user 2, confirmed orders and partners named `Amber …` (one in ten), and
/// the core tables the service writes (idempotency keys).
async fn setup(wire: bool) -> (Harness, Vec<i64>, Vec<i64>, Vec<i64>) {
    let mut builder = Harness::builder([&base_users::ADDON, &bench::ADDON]);
    if wire {
        builder = builder.count_round_trips();
    }
    let h = builder.build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'admin', 'admin', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let orders = bench_fixture::dataset(&h, ORDERS, 10, 100, 50).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "UPDATE bench_order SET state = 'confirmed'; \
         UPDATE bench_partner SET name = 'Amber ' || code WHERE id % 10 = 1",
    )
    .await
    .unwrap();
    let lines: Vec<i64> = tx
        .query("SELECT id FROM bench_order_line ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let partners: Vec<i64> = tx
        .query("SELECT id FROM bench_partner ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    tx.commit().await.unwrap();
    let report = h.orm().sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    (h, orders, lines, partners)
}

/// One BENCH-007 kind (`erp-bench` `serve::request::mix_request`), request `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Form,
    List,
    NameSearch,
    Save,
    Create,
}

const KINDS: [Kind; 5] = [
    Kind::Form,
    Kind::List,
    Kind::NameSearch,
    Kind::Save,
    Kind::Create,
];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Form => "form",
            Kind::List => "list",
            Kind::NameSearch => "name_search",
            Kind::Save => "save",
            Kind::Create => "create",
        }
    }

    /// Path and body of request `i` (a save writes a new value each time, a create a new code).
    fn request(
        self,
        i: usize,
        order: i64,
        line: i64,
        partner: i64,
        limit: u64,
    ) -> (&'static str, Vec<u8>) {
        let (path, body) = match self {
            Kind::List => (
                "/json/2/bench.order/web_search_read",
                json!({
                    "domain": [["state", "=", "confirmed"]],
                    "specification": {"code": {}, "name": {}, "partner_id": {"fields": {"display_name": {}}},
                                      "date_order": {}, "amount_total": {}, "state": {}},
                    "limit": limit,
                    "context": context_json(),
                }),
            ),
            Kind::Form => (
                "/json/2/bench.order/web_read",
                json!({"ids": [order], "specification": spec_002_json(), "context": context_json()}),
            ),
            Kind::NameSearch => (
                "/json/2/bench.partner/name_search",
                json!({"name": "amber", "limit": 8, "context": context_json()}),
            ),
            Kind::Save => (
                "/json/2/bench.order.line/write",
                json!({"ids": [line], "vals": {"product_name": format!("Edited {i}")},
                       "context": context_json()}),
            ),
            Kind::Create => {
                let code = format!("XA-{i}");
                (
                    "/json/2/bench.order/create",
                    json!({"vals_list": [{
                        "code": code, "name": format!("SO-{code}"), "partner_id": partner,
                        "line_ids": [
                            [0, 0, {"code": format!("{code}-1"), "product_name": "New", "qty": 1, "price_unit": 10.0}],
                            [0, 0, {"code": format!("{code}-2"), "product_name": "New", "qty": 2, "price_unit": 5.5}],
                        ],
                    }], "context": context_json()}),
                )
            }
        };
        (path, serde_json::to_vec(&body).unwrap())
    }
}

/// `bench/workloads.json` `bench_002_specification`.
fn spec_002_json() -> serde_json::Value {
    json!({"code": {}, "name": {}, "state": {}, "date_order": {}, "amount_total": {},
           "partner_name": {}, "partner_id": {"fields": {"code": {}}},
           "line_ids": {"fields": {"code": {}, "qty": {}, "price_unit": {}, "price_subtotal": {}}},
           "tag_ids": {"fields": {"code": {}}}})
}

/// The list page's specification, built in Rust.
fn list_spec() -> Spec {
    Spec::new()
        .field("code")
        .field("name")
        .with(
            "partner_id",
            SpecItem::fields(Spec::new().field("display_name")),
        )
        .field("date_order")
        .field("amount_total")
        .field("state")
}

/// Per-request figures of `RpcService::call` for `kind`: requests `from..from + n` (prepared
/// outside the measurement: the body as the transport receives it, the caller as it built it).
fn calls(
    rt: &tokio::runtime::Runtime,
    service: &RpcService,
    data: &Data,
    kind: Kind,
    from: usize,
    n: usize,
    limit: u64,
) -> (Sum, u64) {
    let mut sum = Sum::default();
    let mut statements = 0;
    let inputs: Vec<_> = (from..from + n)
        .map(|i| {
            let (path, body) = kind.request(
                i,
                data.orders[i % data.orders.len()],
                data.lines[i % data.lines.len()],
                data.partners[i % data.partners.len()],
                limit,
            );
            (path, body, Caller::Principal(principal()))
        })
        .collect();
    for (path, body, caller) in inputs {
        let response = measured(&mut sum, rt, || {
            let response = rt.block_on(service.call(RpcCall::json2(path, body, caller)));
            assert_eq!(
                response.status(),
                200,
                "{}: {}",
                kind.name(),
                String::from_utf8_lossy(response.body())
            );
            let statements = response.statements();
            drop(response);
            statements
        });
        statements = response;
    }
    assert!(
        sum.delivered >= n as u64,
        "database-backed RPC windows must count delivered batches"
    );
    (sum, statements)
}

struct Data {
    orders: Vec<i64>,
    lines: Vec<i64>,
    partners: Vec<i64>,
}

/// The same work as the form read and the list page, called directly on the ORM, split by layer.
#[derive(Debug, Default)]
struct Direct {
    begin: Sum,
    open: Sum,
    method: Sum,
    write: Sum,
    commit: Sum,
    statement: Sum,
}

fn direct(rt: &tokio::runtime::Runtime, h: &Harness, data: &Data, kind: Kind, n: usize) -> Direct {
    let mut d = Direct::default();
    let entry: Arc<str> = Arc::from(match kind {
        Kind::List => "bench.order.web_search_read",
        _ => "bench.order.web_read",
    });
    let domain = Domain::from_json(&json!([["state", "=", "confirmed"]])).unwrap();
    for i in 0..n {
        // prepared outside: what the binder hands the method (parsed specification, domain)
        let p = principal();
        let ctx = context();
        let entry = Arc::clone(&entry);
        let spec = match kind {
            Kind::List => list_spec(),
            _ => bench_fixture::spec_002(),
        };
        let args = (kind == Kind::List).then(|| {
            let mut a = WebSearchRead::new(domain.clone(), spec.clone());
            a.limit = Some(80);
            a
        });
        let order = data.orders[i % data.orders.len()];
        // begin: options and the pool checkout; the opening flight is the next layer (the served
        // path opens on its first statement instead, through the same `start` at the same cost)
        let mut tx = measured(&mut d.begin, rt, || {
            rt.block_on(
                h.orm()
                    .begin_opts(BeginOptions::for_principal(p).context(ctx).read_only(entry)),
            )
            .unwrap()
        });
        measured(&mut d.open, rt, || {
            rt.block_on(tx.open()).unwrap();
        });
        // one statement on the warm connection, the ORM's raw statement path (cached prepare)
        measured(&mut d.statement, rt, || {
            let rows = rt
                .block_on(tx.query("SELECT code FROM bench_order WHERE id = $1", &[&order]))
                .unwrap();
            assert_eq!(rows.len(), 1);
        });
        let value = measured(&mut d.method, rt, || {
            rt.block_on(async {
                let model = tx.env().records("bench.order").unwrap();
                match args {
                    Some(args) => model.web_search_read(&mut tx, args).await.unwrap(),
                    None => {
                        let rows = model
                            .browse([order])
                            .unwrap()
                            .web_read(&mut tx, spec)
                            .await
                            .unwrap();
                        Value::list(rows)
                    }
                }
            })
        });
        measured(&mut d.write, rt, || {
            let body = erp_rpc::__output::serialize(&value, 1 << 30).unwrap();
            assert!(!body.is_empty());
        });
        drop(value);
        measured(&mut d.commit, rt, || {
            rt.block_on(tx.commit_pipelined(None)).unwrap();
        });
    }
    d
}

/// SPEC-free budget guard (perf infrastructure, as PERF.SERVE-FLOOR): per-request allocations of
/// today's served path per BENCH-007 kind, and the layer split of the form read and list page.
fn rpc_kinds_allocate_within_todays_ceilings() {
    let rt = runtime();
    let (h, orders, lines, partners) = rt.block_on(setup(false));
    let data = Data {
        orders,
        lines,
        partners,
    };
    let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
    let mut figures = Vec::new();
    for kind in KINDS {
        calls(&rt, &service, &data, kind, 0, WARM, 80);
        let (sum, statements) = calls(&rt, &service, &data, kind, WARM, MEASURED, 80);
        println!("{} (statements {statements})", sum.line(kind.name()));
        figures.push((kind, sum));
    }
    // the rejection: rendering a transport-detected refusal (no statement, nothing parsed)
    let mut reject = Sum::default();
    for _ in 0..WARM + MEASURED {
        measured(&mut reject, &rt, || {
            let r = service.reject(RpcFailure::from(InfraError::Overloaded { retry_after: 1 }));
            assert_eq!(r.status(), 503);
        });
    }
    println!("{}", reject.line("rejection"));

    for kind in [Kind::Form, Kind::List] {
        direct(&rt, &h, &data, kind, WARM);
        let d = direct(&rt, &h, &data, kind, MEASURED);
        let whole = figures.iter().find(|(k, _)| *k == kind).unwrap().1;
        let layers = d.begin.blocks()
            + d.open.blocks()
            + d.method.blocks()
            + d.write.blocks()
            + d.commit.blocks();
        let layers_bytes = d.begin.bytes()
            + d.open.bytes()
            + d.method.bytes()
            + d.write.bytes()
            + d.commit.bytes();
        println!("{} direct:", kind.name());
        println!("  {}", d.begin.line("begin (options, checkout)"));
        println!("  {}", d.open.line("open (opening flight)"));
        println!("  {}", d.statement.line("one statement (warm, 1 row)"));
        println!("  {}", d.method.line("method (ORM, method chain)"));
        println!("  {}", d.write.line("result writer"));
        println!("  {}", d.commit.line("commit (COMMIT flight)"));
        println!(
            "  residual = whole − layers (not additive): {:.2} blocks, {:.0} B per request",
            whole.blocks() - layers,
            whole.bytes() - layers_bytes
        );
    }

    // Net block ceilings are pinned to observed per-request maxima; bytes remain raw.
    for (kind, sum) in &figures {
        let bytes = match kind {
            Kind::Form => CEIL_FORM_BYTES,
            Kind::List => CEIL_LIST_BYTES,
            Kind::NameSearch => CEIL_NAME_SEARCH_BYTES,
            Kind::Save => CEIL_SAVE_BYTES,
            Kind::Create => CEIL_CREATE_BYTES,
        };
        let blocks = block_ceiling(*kind);
        assert!(
            sum.blocks() <= blocks && sum.bytes() <= bytes,
            "{}",
            sum.line(kind.name())
        );
    }
    assert!(
        reject.blocks() <= CEIL_REJECT && reject.bytes() <= CEIL_REJECT_BYTES,
        "{}",
        reject.line("rejection")
    );
    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}

/// The list page at 20 and 80 order rows: the cost per added order row through the whole call.
/// Each order row brings one partner row, so one order row is two fetched rows, and's
/// `c_row` (per fetched row; interim ceiling 3, gate 0) is half the printed figure.
fn rpc_list_page_cost_per_row() {
    let rt = runtime();
    let (h, orders, lines, partners) = rt.block_on(setup(false));
    let data = Data {
        orders,
        lines,
        partners,
    };
    let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
    calls(&rt, &service, &data, Kind::List, 0, WARM, 20);
    calls(&rt, &service, &data, Kind::List, 0, WARM, 80);
    let (small, _) = calls(&rt, &service, &data, Kind::List, 0, MEASURED, 20);
    let (large, _) = calls(&rt, &service, &data, Kind::List, 0, MEASURED, 80);
    let per_row = (large.blocks() - small.blocks()) / 60.0;
    let per_row_bytes = (large.bytes() - small.bytes()) / 60.0;
    println!("{}", small.line("list 20 rows"));
    println!("{}", large.line("list 80 rows"));
    println!("per added order row: {per_row:.2} blocks, {per_row_bytes:.0} B");
    assert!(
        per_row <= CEIL_LIST_PER_ROW,
        "{per_row:.6} net blocks per row"
    );
    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}

/// Round trips, statements and `Parse` messages per kind: one request after [`WARM`] warm-up
/// requests, so every connection of the pool has its statements prepared (the audit's
/// round-trip figures come from here).
fn rpc_kinds_round_trips() {
    let rt = runtime();
    let (h, orders, lines, partners) = rt.block_on(setup(true));
    let data = Data {
        orders,
        lines,
        partners,
    };
    let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
    let wire = h.wire().unwrap();
    let mut seen = Vec::new();
    for kind in KINDS {
        calls(&rt, &service, &data, kind, 0, WARM, 80);
        rt.block_on(wire.settled());
        wire.reset();
        let (_, statements) = calls(&rt, &service, &data, kind, WARM, 1, 80);
        rt.block_on(wire.settled());
        let m = wire.messages();
        println!(
            "{}: round trips {}, statements {statements}, Parse {} Bind {} Execute {} Sync {} Query {}",
            kind.name(),
            wire.count(),
            m.parse,
            m.bind,
            m.execute,
            m.sync,
            m.query
        );
        seen.push((kind, wire.count(), statements, m.parse));
    }
    for (kind, turns, statements, parse) in seen {
        let want = match kind {
            Kind::Form => ROUND_TRIPS_FORM,
            Kind::List => ROUND_TRIPS_LIST,
            Kind::NameSearch => ROUND_TRIPS_NAME_SEARCH,
            Kind::Save => ROUND_TRIPS_SAVE,
            Kind::Create => ROUND_TRIPS_CREATE,
        };
        assert_eq!(
            (turns, statements, parse),
            want,
            "{}: (round trips, statements, Parse)",
            kind.name()
        );
    }
    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}

// Ceilings on the mean per request, without the exact delivered-plus-parked driver blocks.
// The old batch margin is gone. Byte bounds still count every allocation, including driver
// nodes; DriverBlocks measures blocks only. No byte bound is raised. These are regression
// ratchets, not's eventual release ceilings or a performance claim.
const CEIL_FORM: f64 = 1_194.0;
const CEIL_FORM_BYTES: f64 = 352_000.0;
const CEIL_LIST: f64 = 1_072.0;
const CEIL_LIST_BYTES: f64 = 290_000.0;
const CEIL_NAME_SEARCH: f64 = 227.0;
const CEIL_NAME_SEARCH_BYTES: f64 = 68_700.0;
const CEIL_SAVE: f64 = 231.0;
const CEIL_SAVE_BYTES: f64 = 69_000.0;
const CEIL_CREATE: f64 = 987.0;
const CEIL_CREATE_BYTES: f64 = 155_500.0;
const CEIL_REJECT: f64 = 3.0;
const CEIL_REJECT_BYTES: f64 = 332.0;

/// Keep the measured Linux ratchets intact. The first macOS CI run measured a
/// distinct net envelope, recorded in github-macos-failure.txt. Each macOS cap
/// is its observed per-request maximum, below that kind's original raw budget.
/// No platform-specific amount is subtracted from a measurement.
fn block_ceiling(kind: Kind) -> f64 {
    match std::env::consts::OS {
        "linux" => match kind {
            Kind::Form => CEIL_FORM,
            Kind::List => CEIL_LIST,
            Kind::NameSearch => CEIL_NAME_SEARCH,
            Kind::Save => CEIL_SAVE,
            Kind::Create => CEIL_CREATE,
        },
        "macos" => match kind {
            Kind::Form => 1_196.0,
            Kind::List => 1_075.0,
            Kind::NameSearch => 229.0,
            Kind::Save => 233.0,
            Kind::Create => 989.0,
        },
        host => panic!(
            "RPC allocation budgets are not measured for {host}; calibrate this test before enabling it"
        ),
    }
}

/// Per added order row (two fetched rows, order and partner), after normalizing each size's
/// own window. Observed net extrema are 684 blocks at 20 rows and 1072 at 80 rows; their
/// difference over 60 rows pins the envelope. No driver term is estimated from a row count.
const CEIL_LIST_PER_ROW: f64 = (1_072.0 - 684.0) / 60.0;
/// (round trips, statements, `Parse` messages) of one warm request. `Parse` is not zero on the
/// list and name_search: their statements carry domain literals and run as unnamed statements
/// planned per call (`PlanPolicy::PerCall`), in the same round trip.
const ROUND_TRIPS_FORM: (u64, u64, u64) = (7, 5, 0);
const ROUND_TRIPS_LIST: (u64, u64, u64) = (5, 3, 2);
const ROUND_TRIPS_NAME_SEARCH: (u64, u64, u64) = (3, 1, 1);
// The statement counts (`.1`) of the two writes include the audit INSERT of the commit preamble
// (one INSERT per mutating attempt). The round-trip
// pins (`.0`) belong to the DEFERRED_PERFORMANCE wire-cost test and are not re-measured here.
const ROUND_TRIPS_SAVE: (u64, u64, u64) = (3, 2, 0);
const ROUND_TRIPS_CREATE: (u64, u64, u64) = (5, 4, 0);

fn rpc_kinds_semantics() {
    let rt = runtime();
    let (h, orders, lines, partners) = rt.block_on(setup(false));
    let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
    for kind in KINDS {
        let (path, body) = kind.request(0, orders[0], lines[0], partners[0], 80);
        let response =
            rt.block_on(service.call(RpcCall::json2(path, body, Caller::Principal(principal()))));
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        // Method statement counts exclude Parse messages and transaction control;
        // retain this contract independently of the deferred warm wire-cost test.
        let expected_statements = match kind {
            Kind::Form => ROUND_TRIPS_FORM.1,
            Kind::List => ROUND_TRIPS_LIST.1,
            Kind::NameSearch => ROUND_TRIPS_NAME_SEARCH.1,
            Kind::Save => ROUND_TRIPS_SAVE.1,
            Kind::Create => ROUND_TRIPS_CREATE.1,
        };
        assert_eq!(
            response.statements(),
            expected_statements,
            "{}",
            kind.name()
        );
    }
    let (path, body) = Kind::List.request(0, orders[0], lines[0], partners[0], 20);
    let response =
        rt.block_on(service.call(RpcCall::json2(path, body, Caller::Principal(principal()))));
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    assert_eq!(response.statements(), ROUND_TRIPS_LIST.1, "list 20 rows");
    assert_eq!(
        service
            .reject(RpcFailure::from(InfraError::Overloaded { retry_after: 1 }))
            .status(),
        503
    );
    for kind in [Kind::Form, Kind::List] {
        rt.block_on(async {
            let entry: Arc<str> = Arc::from(match kind {
                Kind::List => "bench.order.web_search_read",
                _ => "bench.order.web_read",
            });
            let mut tx = h
                .orm()
                .begin_opts(
                    BeginOptions::for_principal(principal())
                        .context(context())
                        .read_only(entry),
                )
                .await
                .unwrap();
            tx.open().await.unwrap();
            assert_eq!(
                tx.query("SELECT code FROM bench_order WHERE id = $1", &[&orders[0]])
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let model = tx.env().records("bench.order").unwrap();
            let value = if kind == Kind::List {
                let domain = Domain::from_json(&json!([["state", "=", "confirmed"]])).unwrap();
                let mut args = WebSearchRead::new(domain, list_spec());
                args.limit = Some(80);
                model.web_search_read(&mut tx, args).await.unwrap()
            } else {
                Value::list(
                    model
                        .browse([orders[0]])
                        .unwrap()
                        .web_read(&mut tx, bench_fixture::spec_002())
                        .await
                        .unwrap(),
                )
            };
            assert!(
                !erp_rpc::__output::serialize(&value, 1 << 30)
                    .unwrap()
                    .is_empty()
            );
            tx.commit_pipelined(None).await.unwrap();
        });
    }
    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}
