//! Same existing company default lookup on main and BASE; normal allocator, remote Linux only.
#![allow(clippy::print_stdout, reason = "measurement producer")]
use erp_fields::{Value, convert::value_to_json};
use erp_test_models::base_users;
use erp_testkit::Harness;
use std::{error::Error, sync::Arc, time::Instant};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn peak() -> Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or("VmHWM is absent")?;
    let mut fields = value.split_whitespace();
    let kb = fields
        .next()
        .ok_or("VmHWM value is absent")?
        .parse::<u64>()?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        return Err("invalid VmHWM unit/shape".into());
    }
    if kb == 0 {
        return Err("VmHWM cannot be zero for a running process".into());
    }
    kb.checked_mul(1024)
        .ok_or_else(|| "VmHWM byte count overflow".into())
}

fn main() -> Result<()> {
    let operations: usize = std::env::args()
        .nth(1)
        .ok_or("operation count required")?
        .parse()?;
    if !(1..=10_000).contains(&operations) {
        return Err("operations must be 1..=10000".into());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let harness =
        rt.block_on(Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON]).build())?;
    let mut metadata = rt.block_on(harness.begin())?;
    let versions = rt.block_on(metadata.query("SELECT version()", &[]))?;
    let [version] = versions.as_slice() else {
        return Err("database version query must return one row".into());
    };
    let postgres_version: String = version.try_get(0)?;
    rt.block_on(metadata.rollback())?;
    let mut expected = None;
    for _ in 0..3 {
        let mut tx = rt.block_on(harness.begin())?;
        rt.block_on(tx.open())?;
        let values = rt.block_on(
            tx.env()
                .records("res.company")?
                .default_get(&mut tx, &["name", "parent_id"]),
        )?;
        rt.block_on(tx.flush())?;
        if let Some(previous) = &expected {
            if previous != &values {
                return Err("warmup default map changed".into());
            }
        } else {
            expected = Some(values);
        }
        rt.block_on(tx.rollback())?;
    }
    let expected = expected.ok_or("warmup map missing")?;
    let actual_map = value_to_json(&Value::Map(Arc::new(expected.clone())))?;
    let mut queries = Vec::with_capacity(operations);
    let mut elapsed = 0u128;
    let setup_peak = peak()?;
    std::fs::write("/proc/self/clear_refs", "5")?;
    for _ in 0..operations {
        let mut tx = rt.block_on(harness.begin())?;
        rt.block_on(tx.open())?;
        let before = tx.query_count();
        let start = Instant::now();
        let values = rt.block_on(async {
            let values = tx
                .env()
                .records("res.company")?
                .default_get(&mut tx, &["name", "parent_id"])
                .await?;
            tx.flush().await?;
            Ok::<_, erp_fields::Error>(values)
        })?;
        elapsed = elapsed
            .checked_add(start.elapsed().as_nanos())
            .ok_or("elapsed overflow")?;
        queries.push(
            tx.query_count()
                .checked_sub(before)
                .ok_or("query counter decreased")?,
        );
        if values != expected {
            return Err("measured map differs from actual warmup map".into());
        }
        rt.block_on(tx.rollback())?;
    }
    let workload_peak = peak()?;
    println!(
        "{}",
        serde_json::json!({
            "case":"company_existing_defaults", "requested_fields":["name","parent_id"],
            "operations":operations, "warmups":3, "elapsed_nanoseconds":elapsed,
            "query_counts":queries, "actual_default_map":actual_map, "postgres_version":postgres_version,
            "setup_peak_bytes":setup_peak, "workload_resident_peak_bytes":workload_peak,
            "time_scope":"default_get+flush only; open/rollback/setup/map verification excluded; ORM cache cold",
            "memory_source":"Linux VmHWM reset after setup/warmup; absolute resident peak includes retained setup and transaction/map verification",
            "fixture":"empty base+base_users harness; identical requested existing fields on both revisions"
        })
    );
    rt.block_on(harness.cleanup())?;
    Ok(())
}
