//! Allocations per JSON-2 request under a compiled policy with private-snapshot plan
//! policy (PERF.ACCESS-SERVE, F14, §5).
//!
//! Measures the BENCH-007 kinds (form, list, name_search, save, create) next to
//! `alloc_rpc_kinds.rs`, with record rules and groups on the bench models (`bench.order`,
//! `bench.order.line`, `bench.partner`, `bench.tag`), compiled into a policy snapshot with
//! origin `SnapshotOrigin::Private` so that every statement uses the private-snapshot
//! `PlanPolicy::PerCall`.
//!
//! ** scheduling:** Benchmark allocation measurement is deferred to milestone end
//! (`DEFERRED_PERFORMANCE`), while semantic coverage runs in ordinary CI.
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

use erp_orm::{FixedPolicy, GroupId, Orm, Principal, PrincipalBuilder, SnapshotOrigin};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_sdk::UserId;
use erp_security::resolver::Resolver;
use erp_security::rows::{GroupRow, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;
use erp_test_models::{base_users, bench};
use erp_testkit::Harness;
use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::DriverBlocks;
use serde_json::json;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    rpc_compiled_policy_kinds_allocate,
    rpc_compiled_policy_kinds_semantics,
);

const WARM: usize = 64;
const MEASURED: usize = 200;
const ORDERS: usize = 300;

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

fn measured<R>(sum: &mut Sum, rt: &tokio::runtime::Runtime, work: impl FnOnce() -> R) -> R {
    let before = erp_orm::ResponseBatches::process_wide();
    let (stats, out) = alloc::measure(work).expect("measure");
    let window = erp_orm::ResponseBatches::process_wide()
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

fn context_json() -> serde_json::Value {
    json!({"tz": "UTC", "lang": "en_US"})
}

fn spec_002_json() -> serde_json::Value {
    json!({"code": {}, "name": {}, "state": {}, "date_order": {}, "amount_total": {},
           "partner_name": {}, "partner_id": {"fields": {"code": {}}},
           "line_ids": {"fields": {"code": {}, "qty": {}, "price_unit": {}, "price_subtotal": {}}},
           "tag_ids": {"fields": {"code": {}}}})
}

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

struct Data {
    orders: Vec<i64>,
    lines: Vec<i64>,
    partners: Vec<i64>,
}

async fn setup(wire: bool) -> (Harness, Data, Orm) {
    let mut builder = Harness::builder([&base_users::ADDON, &bench::ADDON]);
    if wire {
        builder = builder.count_round_trips();
    }
    let h = builder.build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "ALTER TABLE res_users ADD COLUMN IF NOT EXISTS company_id int8; \
         CREATE TABLE IF NOT EXISTS res_groups_users_rel (uid int8 NOT NULL, gid int8 NOT NULL); \
         CREATE TABLE IF NOT EXISTS res_company_users_rel (user_id int8 NOT NULL, cid int8 NOT NULL); \
         INSERT INTO res_users (id, name, login, active) VALUES (2, 'admin', 'admin', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2); \
         INSERT INTO res_groups_users_rel (uid, gid) VALUES (2, 100);",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let orders = bench_fixture::dataset(&h, ORDERS, 10, 100, 50).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "UPDATE bench_order SET state = 'confirmed'; \
         UPDATE bench_partner SET name = 'Amber ' || code WHERE id % 10 = 1;",
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

    let gid = GroupId::new(100).unwrap();
    let group = GroupRow::new(gid, "Internal User")
        .unwrap()
        .with_xmlid(erp_orm::QualifiedXmlId::parse("base.group_user").unwrap());
    let r1 = RuleRow::new(RuleId::new(1).unwrap(), "order_user", "bench.order", "crud")
        .unwrap()
        .with_group(gid)
        .with_domain("[('state', '!=', 'cancelled')]");

    let r2 = RuleRow::new(
        RuleId::new(2).unwrap(),
        "line_user",
        "bench.order.line",
        "crud",
    )
    .unwrap()
    .with_group(gid)
    .with_domain("[('id', '>', 0)]");

    let r3 = RuleRow::new(
        RuleId::new(3).unwrap(),
        "partner_user",
        "bench.partner",
        "crud",
    )
    .unwrap()
    .with_group(gid)
    .with_domain("[('id', '>', 0)]");

    let r4 = RuleRow::new(RuleId::new(4).unwrap(), "tag_user", "bench.tag", "crud")
        .unwrap()
        .with_group(gid)
        .with_domain("[('id', '>', 0)]");

    let rows = PolicyRows::builder(1)
        .groups([group])
        .rules([r1, r2, r3, r4])
        .build()
        .unwrap();

    let snapshot = Compiler::new(Arc::clone(h.registry()))
        .origin(SnapshotOrigin::Private)
        .compile(&rows)
        .unwrap();

    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(FixedPolicy::new(Arc::new(snapshot)));
    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let orm = erp_orm::Orm::builder(Arc::clone(h.registry()), pool())
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();

    let data = Data {
        orders,
        lines,
        partners,
    };
    (h, data, orm)
}

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
    (sum, statements)
}

/// Semantic coverage for BENCH-007 kinds under the compiled policy and private-snapshot plan policy.
fn rpc_compiled_policy_kinds_semantics() {
    let rt = runtime();
    let (h, data, orm) = rt.block_on(setup(false));
    let service = RpcService::new(orm, Host, RpcConfig::default());

    // 1. All BENCH-007 kinds succeed under the compiled policy
    for kind in KINDS {
        let (path, body) = kind.request(0, data.orders[0], data.lines[0], data.partners[0], 80);
        let response =
            rt.block_on(service.call(RpcCall::json2(path, body, Caller::Principal(principal()))));
        assert_eq!(
            response.status(),
            200,
            "{}: {}",
            kind.name(),
            String::from_utf8_lossy(response.body())
        );
        assert!(
            response.statements() > 0,
            "{}: statement count",
            kind.name()
        );
    }

    // 2. Access rule enforcement check:
    // bench.order has the rule [('state', '!=', 'cancelled')]. Cancelling an order must deny access.
    rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        tx.execute(
            "UPDATE bench_order SET state = 'cancelled' WHERE id = $1",
            &[&data.orders[0]],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    });

    let (path, body) = Kind::Form.request(0, data.orders[0], data.lines[0], data.partners[0], 80);
    let response =
        rt.block_on(service.call(RpcCall::json2(path, body, Caller::Principal(principal()))));
    assert_eq!(
        response.status(),
        403,
        "cancelled order must be denied by compiled record rule, got status {}",
        response.status()
    );
    // a denial is never memoized: the same read is refused again
    let (path, body) = Kind::Form.request(0, data.orders[0], data.lines[0], data.partners[0], 80);
    let response =
        rt.block_on(service.call(RpcCall::json2(path, body, Caller::Principal(principal()))));
    assert_eq!(response.status(), 403, "second read of the cancelled order");

    // 3. A list over every order returns the open orders and leaves the cancelled one out (the
    // order `Kind::Create` made in step 1 is open too and is listed as well).
    let body = serde_json::to_vec(&json!({
        "domain": [],
        "specification": {"state": {}},
        "limit": ORDERS + 10,
        "context": context_json(),
    }))
    .unwrap();
    let response = rt.block_on(service.call(RpcCall::json2(
        "/json/2/bench.order/web_search_read",
        body,
        Caller::Principal(principal()),
    )));
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let decoded: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let listed: Vec<i64> = decoded["records"]
        .as_array()
        .unwrap_or_else(|| panic!("no records array: {decoded}"))
        .iter()
        .map(|r| r["id"].as_i64().unwrap_or_else(|| panic!("no id: {r}")))
        .collect();
    assert_eq!(decoded["length"].as_u64(), Some(listed.len() as u64));
    assert!(!listed.contains(&data.orders[0]), "cancelled order listed");
    let mut fixture: Vec<i64> = listed
        .iter()
        .copied()
        .filter(|id| data.orders.contains(id))
        .collect();
    fixture.sort_unstable();
    let mut open: Vec<i64> = data.orders[1..].to_vec();
    open.sort_unstable();
    assert_eq!(fixture, open);
    // only the order step 1 created is listed besides the fixture
    assert_eq!(listed.len(), open.len() + 1);

    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}

/// Allocations per JSON-2 request under the compiled policy (deferred).
fn rpc_compiled_policy_kinds_allocate() {
    let rt = runtime();
    let (h, data, orm) = rt.block_on(setup(false));
    let service = RpcService::new(orm, Host, RpcConfig::default());

    let mut figures = Vec::new();
    for kind in KINDS {
        calls(&rt, &service, &data, kind, 0, WARM, 80);
        let (sum, _) = calls(&rt, &service, &data, kind, WARM, MEASURED, 80);
        println!("{}", sum.line(kind.name()));
        figures.push((kind, sum));
    }

    drop(service);
    rt.block_on(h.cleanup()).unwrap();
}
