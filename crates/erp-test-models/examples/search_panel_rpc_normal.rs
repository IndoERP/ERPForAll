//! Normal-system-allocator RpcService time and Linux endpoint-window absolute RSS probe.
//! Usage: search_panel_rpc_normal 24|10000 range|multi core|sql
//! One mode per process. Same service/fixture/response as the allocation harness, no wire proxy.
//! VmHWM includes retained baseline state;
//! it is not a heap delta. Endpoint status reads and request preparation are inside the RSS
//! window, while each latency covers only RpcService::call.
#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    reason = "standalone measurement probe; fixed fixture invariants have explicit messages"
)]

use erp_fields::rows::{Column, ColumnData, Key, Layout, RowsBuilder};
use erp_orm::{Principal, PrincipalBuilder};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcResponse, RpcService};
use erp_sdk::{UserId, Value, ValueMap};
use erp_test_models::{base_users, test_web};
use erp_testkit::Harness;
use serde_json::json;
use std::{error::Error, fs, io, process::Command, sync::Arc, time::Instant};

type ProbeResult<T> = Result<T, Box<dyn Error>>;

fn refusal(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

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
async fn floor(tx: &mut erp_sdk::Transaction, path: Path) -> erp_sdk::Result<Value> {
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
    let rows = tx.query(sql, &[]).await?;
    let mut keys = vec![Key::Field("id"), Key::Field("display_name")];
    if matches!(path, Path::Multi) {
        keys.extend([Key::Field("group_id"), Key::Field("group_name")]);
    }
    keys.push(Key::Field("__count"));
    let mut values = RowsBuilder::new(Layout::new(keys)?, rows.len())?;
    let decode = |error| {
        erp_sdk::Error::programming(format!("private panel SQL floor result decode: {error}"))
    };
    let mut ids = Vec::with_capacity(rows.len());
    let mut names = Vec::with_capacity(rows.len());
    let mut counts = Vec::with_capacity(rows.len());
    let mut groups = Vec::with_capacity(if matches!(path, Path::Multi) {
        rows.len()
    } else {
        0
    });
    for row in &rows {
        ids.push(row.try_get::<_, i64>(0).map_err(decode)?);
        names.push(Some(Arc::<str>::from(
            row.try_get::<_, &str>(1).map_err(decode)?,
        )));
        counts.push(row.try_get::<_, i64>(3).map_err(decode)?);
        if matches!(path, Path::Multi) {
            groups.push(Some(Arc::<str>::from(
                row.try_get::<_, &str>(2).map_err(decode)?,
            )));
        }
    }
    values.column("id", Column::new(ColumnData::Int(ids)))?;
    values.column("display_name", Column::new(ColumnData::Str(names)))?;
    if matches!(path, Path::Multi) {
        values.column("group_id", Column::new(ColumnData::Str(groups.clone())))?;
        values.column("group_name", Column::new(ColumnData::Str(groups)))?;
    }
    values.column("__count", Column::new(ColumnData::Int(counts)))?;
    let values = Value::from(values.finish()?);
    Ok(match path {
        Path::Range => map([("parent_field", Value::False), ("values", values)]),
        Path::Multi => map([("values", values)]),
    })
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
            super::floor(tx, super::Path::Range).await
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
            super::floor(tx, super::Path::Multi).await
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
        PrincipalBuilder::user(UserId::new(2).expect("fixed uid 2 is valid"))
            .build()
            .expect("fixed user principal is valid"),
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
    (
        route,
        serde_json::to_vec(&body).expect("fixed fixture JSON serializes"),
    )
}

#[derive(Debug)]
struct Memory {
    rss_kib: u64,
    hwm_kib: u64,
}

fn memory() -> ProbeResult<Memory> {
    let status = fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| -> ProbeResult<u64> {
        let mut matching = status.lines().filter(|line| line.starts_with(name));
        let line = matching
            .next()
            .ok_or_else(|| refusal(format!("missing {name} in /proc/self/status")))?;
        if matching.next().is_some() {
            return Err(refusal(format!("duplicate {name} in /proc/self/status")).into());
        }
        let tokens: Vec<_> = line.split_whitespace().collect();
        if tokens.len() != 3 || tokens[0] != name || tokens[2] != "kB" {
            return Err(refusal(format!("invalid {name} format: {line}")).into());
        }
        let value: u64 = tokens[1].parse()?;
        if value == 0 {
            return Err(refusal(format!("zero {name} in /proc/self/status")).into());
        }
        Ok(value)
    };
    let memory = Memory {
        rss_kib: field("VmRSS:")?,
        hwm_kib: field("VmHWM:")?,
    };
    if memory.hwm_kib < memory.rss_kib {
        return Err(refusal("VmHWM is below VmRSS").into());
    }
    Ok(memory)
}

fn stamp(items: usize, path: Path, sql_floor: bool) -> ProbeResult<()> {
    println!(
        "invocation={:?}; cwd={:?}; items={items}; path={path:?}; sql_floor={sql_floor}; allocator=system-default; warmups=8; samples=32; debug_assertions={}",
        std::env::args_os().collect::<Vec<_>>(),
        std::env::current_dir()?,
        cfg!(debug_assertions)
    );
    for (program, args) in [
        ("hostname", vec![]),
        ("date", vec!["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        ("rustc", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("uname", vec!["-a"]),
    ] {
        let output = Command::new(program).args(&args).output()?;
        if !output.status.success() {
            return Err(refusal(format!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        println!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8(output.stdout)?.trim()
        );
    }
    Ok(())
}

fn check(response: &RpcResponse, sql_floor: bool) -> ProbeResult<()> {
    if response.status() != 200 {
        return Err(refusal(format!(
            "RPC status {}: {}",
            response.status(),
            String::from_utf8_lossy(response.body())
        ))
        .into());
    }
    if response.attempts() != 1 {
        return Err(refusal(format!("RPC attempts {}, expected 1", response.attempts())).into());
    }
    let ceiling = if sql_floor { 1 } else { 2 };
    if response.statements() > ceiling {
        return Err(refusal(format!(
            "RPC statements {}, ceiling {ceiling}",
            response.statements()
        ))
        .into());
    }
    Ok(())
}

fn check_fixture_body(response: &RpcResponse, path: Path, items: usize) -> ProbeResult<()> {
    let body: serde_json::Value = serde_json::from_slice(response.body())?;
    let values = body
        .get("values")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| refusal("fixture response values is not an array"))?;
    if values.len() != items {
        return Err(refusal(format!(
            "fixture response has {} values, expected {items}",
            values.len()
        ))
        .into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for value in values {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| refusal("fixture response id is not an integer"))?;
        if id <= 0 || !ids.insert(id) {
            return Err(refusal("fixture response ids must be positive and unique").into());
        }
        let name = value
            .get("display_name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| refusal("fixture display_name is not a string"))?;
        if !name.starts_with("Person ") {
            return Err(refusal(format!("unexpected fixture name {name:?}")).into());
        }
        let expected_count = match path {
            Path::Range => 2,
            Path::Multi => match value.get("group_id").and_then(serde_json::Value::as_str) {
                Some("cool") => 1,
                Some("unknown") => 2,
                other => return Err(refusal(format!("unexpected fixture group {other:?}")).into()),
            },
        };
        if value.get("__count").and_then(serde_json::Value::as_i64) != Some(expected_count) {
            return Err(refusal(format!(
                "fixture count differs from expected {expected_count}"
            ))
            .into());
        }
    }
    Ok(())
}

fn main() -> ProbeResult<()> {
    if !cfg!(target_os = "linux") {
        return Err(refusal(
            "search_panel_rpc_normal requires Linux /proc/self/status and clear_refs",
        )
        .into());
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err(refusal("usage: search_panel_rpc_normal 24|10000 range|multi core|sql").into());
    }
    let items = match args[0].as_str() {
        "24" => 24,
        "10000" => 10_000,
        other => {
            return Err(refusal(format!("invalid items {other:?}; expected 24 or 10000")).into());
        }
    };
    let path = match args[1].as_str() {
        "range" => Path::Range,
        "multi" => Path::Multi,
        other => {
            return Err(refusal(format!("invalid path {other:?}; expected range or multi")).into());
        }
    };
    let sql_floor = match args[2].as_str() {
        "core" => false,
        "sql" => true,
        other => {
            return Err(refusal(format!("invalid floor {other:?}; expected core or sql")).into());
        }
    };
    stamp(items, path, sql_floor)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let h = rt.block_on(
        Harness::builder([&base_users::ADDON, &test_web::ADDON, &sql_floor::ADDON]).build(),
    )?;
    let mut tx = rt.block_on(h.begin())?;
    rt.block_on(tx.batch(&format!(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'panel', 'panel', true); \
         INSERT INTO test_web_person (name, street, active) \
         SELECT 'Person ' || lpad(i::text, 5, '0'), CASE WHEN i % 2 = 0 THEN 'cool' ELSE 'unknown' END, true \
         FROM generate_series(1, {items}) i; \
         INSERT INTO test_web_lesson (name) VALUES ('included'), ('excluded'); \
         INSERT INTO test_web_lesson_attendee_rel (lesson_id, person_id) \
         SELECT l.id, p.id FROM test_web_lesson l CROSS JOIN test_web_person p"
    )))?;
    let version: String = rt.block_on(tx.query("SELECT version()", &[]))?[0].try_get(0)?;
    rt.block_on(tx.commit())?;
    println!(
        "postgres={version}; policy=Harness-default-allow-all; principal=uid2; signal=none; transport=RpcService-only; rss=absolute-endpoint-window"
    );
    let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
    // Both complete serialized results must agree before the measured window.
    let (route, body) = request(path, false);
    let core = rt.block_on(service.call(RpcCall::json2(route, body, caller())));
    check(&core, false)?;
    check_fixture_body(&core, path, items)?;
    let (route, body) = request(path, true);
    let floor = rt.block_on(service.call(RpcCall::json2(route, body, caller())));
    check(&floor, true)?;
    if core.body() != floor.body() {
        return Err(refusal("complete core and SQL floor response bodies differ").into());
    }
    drop(core);
    drop(floor);
    for _ in 0..8 {
        let (route, body) = request(path, sql_floor);
        let response = rt.block_on(service.call(RpcCall::json2(route, body, caller())));
        check(&response, sql_floor)?;
    }
    // Preallocate samples so reporting/storage does not add a growing buffer to this RSS window.
    let mut samples = Vec::with_capacity(32);
    let before = memory()?;
    fs::write("/proc/self/clear_refs", b"5\n")?;
    for round in 0..32 {
        let (route, body) = request(path, sql_floor);
        let call = RpcCall::json2(route, body, caller());
        let start = Instant::now();
        let response = rt.block_on(service.call(call));
        let elapsed_ns = start.elapsed().as_nanos();
        check(&response, sql_floor)?;
        samples.push((
            round,
            elapsed_ns,
            response.statements(),
            response.status(),
            response.attempts(),
        ));
        drop(response);
    }
    let after = memory()?;
    println!(
        "baseline_rss_kib={}; before_hwm_kib={}; endpoint_rss_kib={}; endpoint_window_hwm_kib={}; rss_semantics=absolute-includes-retained-baseline-not-heap-delta",
        before.rss_kib, before.hwm_kib, after.rss_kib, after.hwm_kib
    );
    for (round, elapsed_ns, statements, status, attempts) in samples {
        println!(
            "round={round}; elapsed_ns={elapsed_ns}; statements={statements}; status={status}; attempts={attempts}"
        );
    }
    drop(service);
    rt.block_on(h.cleanup())?;
    println!(
        "measurement_complete=true; response_body_equality=passed; fixture_body_checked=true; hwm_reset=proc-self-clear_refs-5; hwm_reset_complete=true; prewarm_complete=8; samples=32"
    );
    Ok(())
}
