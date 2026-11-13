//! Whole RpcService::call allocation evidence for search-panel core methods and private SQL floors.
//! Both routes use the same service, fixture, JSON kwargs, transaction lifecycle and serializer.
//! The floor replaces the ORM method with one handwritten SQL statement (production uses at most
//! two). Its declarative test-method binder differs from the core binder; it does not optimize
//! RpcService.
//! The principal is uid 2 under an explicit allow-all policy, no signals. A separate singleton
//! pool serves every measured request. Raw client counters, read outside the allocation window
//! without opening transactions, give exact delivered/parked batch deltas. Physical
//! pooled-object identity is checked before/after each call. Only the public driver's per-batch
//! block terms are subtracted; raw bytes/peak remain diagnostic. WireCounter records flights.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use erp_fields::rows::{Column, ColumnData, Key, Layout, RowsBuilder};
use erp_orm::{Orm, Principal, PrincipalBuilder, StaticPolicy};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_sdk::{UserId, Value, ValueMap};
use erp_test_models::{base_users, test_web};
use erp_testkit::deadpool_postgres::Object;
use erp_testkit::driver::{DRIVER_MAX_BLOCKS_PER_BATCH, DRIVER_MIN_BLOCKS_PER_BATCH};
use erp_testkit::{Harness, alloc};
use serde_json::json;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    search_panel_rpc_24_allocations_and_floor,
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    search_panel_rpc_10k_allocations_and_floor,
    search_panel_rpc_24_semantics,
    search_panel_rpc_10k_semantics,
);

// Measured maxima; exact driver blocks excluded.
// Requested bytes and peak are diagnostics, not ratchets: driver buffer refills vary by schedule.
const MEASURED: usize = 32;
const BUDGET_24: [u64; 2] = [525, 802];
const BUDGET_10K: [u64; 2] = [50_699, 71_066];

fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(Arc::new(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<ValueMap>(),
    ))
}

#[derive(Clone, Copy, Debug)]
enum Path {
    Range,
    Multi,
}

// Same visible values and counts as this allow-all, active-person fixture; no access/session
// work is claimed by this floor. The conditional count implements the two custom group domains.
async fn floor(tx: &mut erp_sdk::Transaction, path: Path) -> Value {
    let sql = match path {
        Path::Range => {
            "SELECT p.id, p.name, p.street, count(l.id)::bigint FROM test_web_person p \
            LEFT JOIN test_web_lesson_attendee_rel r ON r.person_id = p.id \
            LEFT JOIN test_web_lesson l ON l.id = r.lesson_id \
            WHERE p.active = true GROUP BY p.id ORDER BY p.id"
        }
        Path::Multi => {
            "SELECT p.id, p.name, p.street, \
            count(l.id) FILTER (WHERE p.street <> 'cool' OR l.name = 'included')::bigint \
            FROM test_web_person p \
            LEFT JOIN test_web_lesson_attendee_rel r ON r.person_id = p.id \
            LEFT JOIN test_web_lesson l ON l.id = r.lesson_id \
            WHERE p.active = true GROUP BY p.id ORDER BY p.id"
        }
    };
    let rows = tx.query(sql, &[]).await.unwrap();
    let mut keys = vec![Key::Field("id"), Key::Field("display_name")];
    if matches!(path, Path::Multi) {
        keys.extend([Key::Field("group_id"), Key::Field("group_name")]);
    }
    keys.push(Key::Field("__count"));
    let mut values = RowsBuilder::new(Layout::new(keys).unwrap(), rows.len()).unwrap();
    let ids = rows.iter().map(|row| row.get::<_, i64>(0)).collect();
    let names = rows
        .iter()
        .map(|row| Some(Arc::<str>::from(row.get::<_, &str>(1))))
        .collect();
    let counts = rows.iter().map(|row| row.get::<_, i64>(3)).collect();
    values
        .column("id", Column::new(ColumnData::Int(ids)))
        .unwrap();
    values
        .column("display_name", Column::new(ColumnData::Str(names)))
        .unwrap();
    if matches!(path, Path::Multi) {
        let groups: Vec<Option<Arc<str>>> = rows
            .iter()
            .map(|row| Some(Arc::<str>::from(row.get::<_, &str>(2))))
            .collect();
        values
            .column("group_id", Column::new(ColumnData::Str(groups.clone())))
            .unwrap();
        values
            .column("group_name", Column::new(ColumnData::Str(groups)))
            .unwrap();
    }
    values
        .column("__count", Column::new(ColumnData::Int(counts)))
        .unwrap();
    let values = Value::from(values.finish().unwrap());
    match path {
        Path::Range => map([("parent_field", Value::False), ("values", values)]),
        Path::Multi => map([("values", values)]),
    }
}

fn stamp() {
    println!(
        "test_invocation={:?}; cwd={:?}",
        std::env::args_os().collect::<Vec<_>>(),
        std::env::current_dir().unwrap()
    );
    for (program, args) in [
        ("hostname", vec![]),
        ("date", vec!["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        ("rustc", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("uname", vec!["-a"]),
    ] {
        let output = Command::new(program).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        println!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8(output.stdout).unwrap().trim()
        );
    }
}

/// This addon exists only in this integration-test executable. Its public-in-registry RPC
/// names are test plumbing, never a production addon/API.
mod sql_floor {
    use erp_sdk::prelude::*;
    use erp_test_models::test_web::Lesson;
    erp_sdk::addon!(name = "test_panel_rpc_floor", depends = ["test_web"]);

    #[extend(Lesson, addon = "test_panel_rpc_floor")]
    pub struct Probe {}

    #[methods(Lesson, addon = "test_panel_rpc_floor")]
    impl Probe {
        #[method]
        #[rpc(effect = read, model)]
        async fn panel_range_sql_floor(
            _this: &RecordSet<Lesson>,
            tx: &mut Transaction,
            field_name: String,
            expand: bool,
            hierarchize: bool,
            enable_counters: bool,
        ) -> Result<Value> {
            if field_name != "attendee_ids" || !expand || hierarchize || !enable_counters {
                return Err(Error::validation(
                    "private panel range floor accepts only its fixed fixture request",
                ));
            }
            Ok(super::floor(tx, super::Path::Range).await)
        }

        #[method]
        #[rpc(effect = read, model)]
        async fn panel_multi_sql_floor(
            _this: &RecordSet<Lesson>,
            tx: &mut Transaction,
            field_name: String,
            expand: bool,
            enable_counters: bool,
            group_by: String,
            group_domain: Value,
        ) -> Result<Value> {
            if field_name != "attendee_ids"
                || !expand
                || !enable_counters
                || group_by != "street"
                || group_domain != super::group_domain()
            {
                return Err(Error::validation(
                    "private panel multi floor accepts only its fixed fixture request",
                ));
            }
            Ok(super::floor(tx, super::Path::Multi).await)
        }
    }
}

fn group_domain() -> Value {
    map([
        (
            "\"cool\"",
            Value::list([Value::list([
                Value::str("name"),
                Value::str("="),
                Value::str("included"),
            ])]),
        ),
        ("\"unknown\"", Value::list([])),
    ])
}

struct Host;
impl RpcHost for Host {
    fn language_active(&self, _code: &str) -> erp_sdk::Result<bool> {
        Ok(false)
    }
    fn has_capability(
        &self,
        _principal: &Principal,
        _capability: &erp_orm::Capability,
    ) -> erp_sdk::Result<bool> {
        Ok(false)
    }
}

fn caller() -> Caller {
    Caller::Principal(
        PrincipalBuilder::user(UserId::new(2).unwrap())
            .build()
            .unwrap(),
    )
}

fn request(path: Path, sql_floor: bool) -> (&'static str, Vec<u8>) {
    let (route, body) = match path {
        Path::Range => (
            if sql_floor {
                "/json/2/test_web.lesson/panel_range_sql_floor"
            } else {
                "/json/2/test_web.lesson/search_panel_select_range"
            },
            json!({"field_name":"attendee_ids", "expand":true, "hierarchize":false, "enable_counters":true}),
        ),
        Path::Multi => (
            if sql_floor {
                "/json/2/test_web.lesson/panel_multi_sql_floor"
            } else {
                "/json/2/test_web.lesson/search_panel_select_multi_range"
            },
            json!({"field_name":"attendee_ids", "expand":true, "enable_counters":true,
                "group_by":"street", "group_domain":{"\"cool\"":[["name","=","included"]], "\"unknown\"":[]}}),
        ),
    };
    (route, serde_json::to_vec(&body).unwrap())
}

fn search_panel_rpc_24_allocations_and_floor() {
    case(24, BUDGET_24);
}
fn search_panel_rpc_10k_allocations_and_floor() {
    case(10_000, BUDGET_10K);
}

fn case(items: usize, budget: [u64; 2]) {
    stamp();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt
        .block_on(
            Harness::builder([&base_users::ADDON, &test_web::ADDON, &sql_floor::ADDON])
                .count_round_trips()
                .build(),
        )
        .unwrap();
    let mut tx = rt.block_on(h.begin()).unwrap();
    rt.block_on(tx.batch(&format!(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'panel', 'panel', true); \
         INSERT INTO test_web_person (name, street, active) \
         SELECT 'Person ' || lpad(i::text, 5, '0'), CASE WHEN i % 2 = 0 THEN 'cool' ELSE 'unknown' END, true \
         FROM generate_series(1, {items}) i; \
         INSERT INTO test_web_lesson (name) VALUES ('included'), ('excluded'); \
         INSERT INTO test_web_lesson_attendee_rel (lesson_id, person_id) \
         SELECT l.id, p.id FROM test_web_lesson l CROSS JOIN test_web_person p"
    ))).unwrap();
    let version: String = rt.block_on(tx.query("SELECT version()", &[])).unwrap()[0].get(0);
    rt.block_on(tx.commit()).unwrap();
    println!(
        "postgres={version}; fixture_items={items}; samples={MEASURED}; debug_assertions={}; policy=allow-all; principal=uid2; driver_blocks=exact-delivered-parked",
        cfg!(debug_assertions)
    );
    let wire = h.wire().unwrap();
    // No other ORM/transaction uses this pool. The harness fixture transaction has committed.
    // Retain the clone only for counter snapshots; never hold its object during service.call.
    let pool = wire.pool(1).unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool.clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let service = RpcService::new(orm, Host, RpcConfig::default());
    let snapshot = || {
        rt.block_on(async {
            let connection = pool.get().await.unwrap();
            (
                Object::id(&connection),
                connection.response_batches(),
                connection.parked_batches(),
            )
        })
    };
    let mut samples = Vec::new();
    for path in [Path::Range, Path::Multi] {
        for sql_floor in [false, true] {
            for _ in 0..8 {
                let (route, body) = request(path, sql_floor);
                let response = rt.block_on(service.call(RpcCall::json2(route, body, caller())));
                assert_eq!(
                    response.status(),
                    200,
                    "{}",
                    String::from_utf8_lossy(response.body())
                );
            }
        }
    }
    for round in 0..MEASURED {
        for (index, path) in [Path::Range, Path::Multi].into_iter().enumerate() {
            let mut bodies = Vec::new();
            for sql_floor in [false, true] {
                let (route, body) = request(path, sql_floor);
                let call = RpcCall::json2(route, body, caller());
                rt.block_on(wire.settled());
                let (before_id, before_delivered, before_parked) = snapshot();
                rt.block_on(wire.settled());
                wire.reset();
                let start = Instant::now();
                let (stats, response) = alloc::measure(|| rt.block_on(service.call(call))).unwrap();
                let elapsed = start.elapsed();
                rt.block_on(wire.settled());
                let flights = wire.count();
                let messages = wire.messages();
                let (after_id, after_delivered, after_parked) = snapshot();
                rt.block_on(wire.settled());
                assert_eq!(
                    wire.count(),
                    flights,
                    "counter snapshot must not send a PostgreSQL request"
                );
                assert_eq!(
                    wire.messages(),
                    messages,
                    "counter snapshot must not send protocol messages"
                );
                assert_eq!(before_id, after_id, "singleton connection was replaced");
                let delivered = after_delivered
                    .checked_sub(before_delivered)
                    .expect("delivered counter must only grow on the same connection");
                let parked = after_parked
                    .checked_sub(before_parked)
                    .expect("parked counter must only grow on the same connection");
                assert!(
                    parked <= delivered,
                    "parked batches exceed delivered batches"
                );
                let parking_blocks = DRIVER_MAX_BLOCKS_PER_BATCH
                    .checked_sub(DRIVER_MIN_BLOCKS_PER_BATCH)
                    .expect("driver maximum must be at least its minimum");
                let driver_blocks = delivered
                    .checked_mul(DRIVER_MIN_BLOCKS_PER_BATCH)
                    .and_then(|blocks| {
                        parked
                            .checked_mul(parking_blocks)
                            .and_then(|parking| blocks.checked_add(parking))
                    })
                    .expect("driver block count overflow");
                let net_blocks = stats
                    .total_blocks
                    .checked_sub(driver_blocks)
                    .expect("counted driver blocks must be inside the measured allocation window");
                println!(
                    "round={round}; path={path:?}; sql_floor={sql_floor}; stats={stats:?}; driver_delivered={delivered}; driver_parked={parked}; net_blocks={net_blocks}; queries={}; flights={flights}; Parse={}; Bind={}; Execute={}; Sync={}; Query={}; elapsed_ns={}",
                    response.statements(),
                    messages.parse,
                    messages.bind,
                    messages.execute,
                    messages.sync,
                    messages.query,
                    elapsed.as_nanos()
                );
                assert_eq!(
                    response.status(),
                    200,
                    "{}",
                    String::from_utf8_lossy(response.body())
                );
                assert_eq!(response.attempts(), 1);
                assert!(response.statements() <= if sql_floor { 1 } else { 2 });
                assert!(
                    flights <= if sql_floor { 3 } else { 4 },
                    "{path:?}: {flights} flights"
                );
                bodies.push(response.body().to_vec());
                if !sql_floor {
                    samples.push((index, net_blocks));
                }
            }
            assert_eq!(
                bodies[0], bodies[1],
                "{path:?}: complete serialized response body differs"
            );
        }
    }
    drop(service);
    pool.close();
    drop(pool);
    rt.block_on(h.cleanup()).unwrap();
    for (index, net_blocks) in samples {
        assert!(
            net_blocks <= budget[index],
            "{index}: {net_blocks} normalized blocks > {}",
            budget[index]
        );
    }
}

#[path = "support/panel_semantics.rs"]
mod panel_semantics;

fn search_panel_rpc_24_semantics() {
    rpc_semantics(24);
}
fn search_panel_rpc_10k_semantics() {
    rpc_semantics(10_000);
}
fn rpc_semantics(items: usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt
        .block_on(
            Harness::builder([&base_users::ADDON, &test_web::ADDON])
                .count_round_trips()
                .build(),
        )
        .unwrap();
    let mut tx = rt.block_on(h.begin()).unwrap();
    rt.block_on(tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'panel', 'panel', true)",
    ))
    .unwrap();
    rt.block_on(tx.batch(&format!(
        "INSERT INTO test_web_person (name, street, active) \
         SELECT 'Person ' || lpad(i::text, 5, '0'), CASE WHEN i % 2 = 0 THEN 'cool' ELSE 'unknown' END, true \
         FROM generate_series(1, {items}) i; \
         INSERT INTO test_web_lesson (name) VALUES ('included'), ('excluded'); \
         INSERT INTO test_web_lesson_attendee_rel (lesson_id, person_id) \
         SELECT l.id, p.id FROM test_web_lesson l CROSS JOIN test_web_person p"
    ))).unwrap();
    let ids: Vec<i64> = rt
        .block_on(tx.query("SELECT id FROM test_web_person ORDER BY id", &[]))
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(ids.len(), items);
    rt.block_on(tx.commit()).unwrap();
    drop(tx);
    let wire = h.wire().unwrap();
    let pool = wire.pool(1).unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool.clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let service = RpcService::new(orm, Host, RpcConfig::default());
    let snapshot = || {
        rt.block_on(async {
            let connection = pool.get().await.unwrap();
            (
                Object::id(&connection),
                connection.response_batches(),
                connection.parked_batches(),
            )
        })
    };
    for path in [Path::Range, Path::Multi] {
        let (route, body) = request(path, false);
        rt.block_on(wire.settled());
        let before = snapshot();
        rt.block_on(wire.settled());
        wire.reset();
        let response = rt.block_on(service.call(RpcCall::json2(route, body, caller())));
        rt.block_on(wire.settled());
        let flights = wire.count();
        let messages = wire.messages();
        let after = snapshot();
        rt.block_on(wire.settled());
        assert_eq!(
            wire.count(),
            flights,
            "counter snapshot must not send a request"
        );
        assert_eq!(
            wire.messages(),
            messages,
            "counter snapshot must not send protocol messages"
        );
        assert_eq!(before.0, after.0, "singleton connection was replaced");
        let delivered = after.1.checked_sub(before.1).unwrap();
        let parked = after.2.checked_sub(before.2).unwrap();
        assert!(parked <= delivered);
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert_eq!(response.attempts(), 1);
        assert!(response.statements() <= 2);
        let actual: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(
            actual,
            panel_semantics::expected(&ids, matches!(path, Path::Multi))
        );
    }
    drop(service);
    pool.close();
    drop(pool);
    rt.block_on(h.cleanup()).unwrap();
}
