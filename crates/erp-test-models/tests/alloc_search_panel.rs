//! Method-layer allocation ratchets and SQL floors for the search-panel RPC entry points.
//! These windows exclude RPC parsing, serialization, transaction opening and fixture creation.
//! One process per test, current-thread runtime, exact driver batch accounting.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "tests"
)]

use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use erp_fields::rows::{Column, ColumnData, Key, Layout, RowsBuilder};
use erp_orm::{SearchPanelSelectMultiRange, SearchPanelSelectRange};
use erp_sdk::{Value, ValueMap};
use erp_test_models::{base_users, test_web};
use erp_testkit::Harness;
use erp_testkit::alloc;
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    search_panel_method_allocations_and_sql_floor,
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    search_panel_10k_method_allocations_and_sql_floor,
    search_panel_method_24_semantics,
    search_panel_method_10k_semantics,
);

// Measured maxima:; exact driver batch blocks excluded.
const RANGE_BUDGET: f64 = 433.0;
const MULTI_BUDGET: f64 = 677.0;
const RANGE_10K_BUDGET: f64 = 50_565.0;
const MULTI_10K_BUDGET: f64 = 70_881.0;

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

fn range_args() -> SearchPanelSelectRange {
    let mut args = SearchPanelSelectRange::new("attendee_ids");
    args.expand = true;
    args.hierarchize = false;
    args.enable_counters = true;
    args
}

fn multi_args() -> SearchPanelSelectMultiRange {
    let mut args = SearchPanelSelectMultiRange::new("attendee_ids");
    args.expand = true;
    args.enable_counters = true;
    args.group_by = Some("street".into());
    args.group_domain = Some(map([
        (
            "\"cool\"",
            Value::list([Value::list([
                Value::str("name"),
                Value::str("="),
                Value::str("included"),
            ])]),
        ),
        ("\"unknown\"", Value::list([])),
    ]));
    args
}

async fn method(tx: &mut erp_sdk::Transaction, path: Path) -> Value {
    let model = tx.env().records("test_web.lesson").unwrap();
    match path {
        Path::Range => model
            .search_panel_select_range(tx, range_args())
            .await
            .unwrap(),
        Path::Multi => model
            .search_panel_select_multi_range(tx, multi_args())
            .await
            .unwrap(),
    }
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

fn search_panel_method_allocations_and_sql_floor() {
    allocation_floor_case(24, RANGE_BUDGET, MULTI_BUDGET);
}

fn search_panel_10k_method_allocations_and_sql_floor() {
    allocation_floor_case(10_000, RANGE_10K_BUDGET, MULTI_10K_BUDGET);
}

fn allocation_floor_case(items: usize, range_budget: f64, multi_budget: f64) {
    stamp();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt
        .block_on(Harness::builder([&base_users::ADDON, &test_web::ADDON]).build())
        .unwrap();
    let mut tx = rt.block_on(h.begin()).unwrap();
    rt.block_on(tx.batch(&format!(
        "INSERT INTO test_web_person (name, street, active) \
         SELECT 'Person ' || lpad(i::text, 5, '0'), CASE WHEN i % 2 = 0 THEN 'cool' ELSE 'unknown' END, true \
         FROM generate_series(1, {items}) i; \
         INSERT INTO test_web_lesson (name) VALUES ('included'), ('excluded'); \
         INSERT INTO test_web_lesson_attendee_rel (lesson_id, person_id) \
         SELECT l.id, p.id FROM test_web_lesson l CROSS JOIN test_web_person p"
    ))).unwrap();
    let version: String = rt.block_on(tx.query("SELECT version()", &[])).unwrap()[0].get(0);
    println!(
        "postgres={version}; fixture_items={items}; debug_assertions={}",
        cfg!(debug_assertions)
    );
    let mut samples = Vec::new();
    for round in 0..3 {
        let mut figures = Vec::with_capacity(4);
        for path in [Path::Range, Path::Multi] {
            for sql_floor in [false, true] {
                // Prepare statements, populate access memoization, then drop record caches so both
                // paths fetch their output during the measured window.
                rt.block_on(async {
                    if sql_floor {
                        floor(&mut tx, path).await;
                    } else {
                        method(&mut tx, path).await;
                    }
                    tx.invalidate_all().await.unwrap();
                });
                let before = tx.query_count();
                let batches_before = tx.driver_response_batches().unwrap();
                let started = Instant::now();
                let (stats, value) = alloc::measure(|| {
                    rt.block_on(async {
                        if sql_floor {
                            floor(&mut tx, path).await
                        } else {
                            method(&mut tx, path).await
                        }
                    })
                })
                .unwrap();
                let elapsed = started.elapsed();
                let batches = DriverBlocks::new(
                    rt.handle(),
                    tx.driver_response_batches()
                        .unwrap()
                        .since(batches_before)
                        .unwrap(),
                )
                .unwrap();
                let queries = tx.query_count() - before;
                let net_blocks = stats.total_blocks as f64 - batches.blocks();
                println!(
                    "round={round}; path={path:?}; sql_floor={sql_floor}; stats={stats:?}; driver={batches}; net_blocks={net_blocks}; queries={queries}; elapsed_ns={}",
                    elapsed.as_nanos()
                );
                assert!(
                    queries <= if sql_floor { 1 } else { 2 },
                    "{queries} queries"
                );
                figures.push((path, sql_floor, stats, net_blocks, value));
            }
        }
        assert_eq!(figures[0].4, figures[1].4, "range SQL floor result");
        assert_eq!(figures[2].4, figures[3].4, "multi SQL floor result");
        for (path, is_floor, stats, net_blocks, _) in figures {
            samples.push((path, is_floor, stats, net_blocks));
        }
    }
    rt.block_on(tx.rollback()).unwrap();
    rt.block_on(h.cleanup()).unwrap();
    for (path, is_floor, _stats, net_blocks) in samples {
        if !is_floor {
            let blocks = match path {
                Path::Range => range_budget,
                Path::Multi => multi_budget,
            };
            assert!(
                net_blocks <= blocks,
                "{path:?}: {net_blocks} net blocks > {blocks}"
            );
        }
    }
}

#[path = "support/panel_semantics.rs"]
mod panel_semantics;

fn search_panel_method_24_semantics() {
    method_semantics(24);
}
fn search_panel_method_10k_semantics() {
    method_semantics(10_000);
}
fn method_semantics(items: usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt
        .block_on(Harness::builder([&base_users::ADDON, &test_web::ADDON]).build())
        .unwrap();
    let mut tx = rt.block_on(h.begin()).unwrap();
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
    for path in [Path::Range, Path::Multi] {
        rt.block_on(tx.invalidate_all()).unwrap();
        let before = tx.query_count();
        let value = rt.block_on(method(&mut tx, path));
        assert!(tx.query_count() - before <= 2);
        let actual: serde_json::Value =
            serde_json::from_slice(&erp_rpc::__output::serialize(&value, 1 << 30).unwrap())
                .unwrap();
        assert_eq!(
            actual,
            panel_semantics::expected(&ids, matches!(path, Path::Multi))
        );
    }
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}
