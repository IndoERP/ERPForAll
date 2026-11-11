//! Normal-allocator BASE operation timing and exact Linux process resident high-water marks.
//! Run remotely from the stamped producer, independently of allocation-test discovery.
#![allow(clippy::print_stdout, reason = "measurement producer")]
#[path = "../tests/base_orm_cost/mod.rs"]
mod base_orm_cost;
use base_orm_cost::{Case, Fixture, Result};
use std::time::Instant;

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
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [name, size, implementation, operations] = args.as_slice() else {
        return Err("usage: base_orm_cost CASE LANGUAGE_ROWS orm|floor OPERATIONS".into());
    };
    let case = Case::parse(name)?;
    let size = size.parse::<usize>()?;
    if matches!(case, Case::LanguageCreate) && size == 10_000 {
        return Err("language create must leave room below the 10000-row campaign cap".into());
    }
    let floor = match implementation.as_str() {
        "orm" => false,
        "floor" => true,
        _ => return Err("implementation must be orm or floor".into()),
    };
    let operations = operations.parse::<usize>()?;
    if !(1..=10_000).contains(&operations) {
        return Err("operations must be 1..=10000".into());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let fixture = rt.block_on(Fixture::new(size))?;
    let mut metadata = rt.block_on(fixture.harness.begin())?;
    let versions = rt.block_on(metadata.query("SELECT version()", &[]))?;
    let [version] = versions.as_slice() else {
        return Err("database version query must return one row".into());
    };
    let postgres_version: String = version.try_get(0)?;
    rt.block_on(metadata.rollback())?;
    let mut total_ns = 0u128;
    let mut queries = Vec::with_capacity(operations);
    for _ in 0..3 {
        let mut tx = rt.block_on(fixture.harness.begin())?;
        rt.block_on(tx.open())?;
        let id = rt.block_on(async {
            if floor {
                fixture.floor(&mut tx, case).await
            } else {
                fixture.orm(&mut tx, case).await
            }
        })?;
        rt.block_on(fixture.verify(&mut tx, case, id))?;
        rt.block_on(tx.rollback())?;
    }
    let setup_peak = peak()?;
    // Refuse if resetting the Linux mark fails: never label cumulative setup peak as phase peak.
    std::fs::write("/proc/self/clear_refs", "5")?;
    for _ in 0..operations {
        let mut tx = rt.block_on(fixture.harness.begin())?;
        rt.block_on(tx.open())?;
        let before = tx.query_count();
        let start = Instant::now();
        let id = rt.block_on(async {
            if floor {
                fixture.floor(&mut tx, case).await
            } else {
                fixture.orm(&mut tx, case).await
            }
        })?;
        total_ns = total_ns
            .checked_add(start.elapsed().as_nanos())
            .ok_or("elapsed sum overflow")?;
        queries.push(tx.query_count() - before);
        rt.block_on(fixture.verify(&mut tx, case, id))?;
        rt.block_on(tx.rollback())?;
    }
    let workload_peak = peak()?;
    println!(
        "{}",
        serde_json::json!({
            "case": name, "language_rows": size, "implementation": implementation,
            "operations": operations, "warmups": 3, "elapsed_nanoseconds": total_ns,
            "postgres_version": postgres_version,
            "query_counts": queries, "setup_peak_bytes": setup_peak,
            "workload_resident_peak_bytes": workload_peak,
            "memory_source": "Linux VmHWM reset to current RSS after setup/warmup; absolute resident peak includes retained setup, verification and transaction boundaries",
            "time_scope": "sum of operation+flush only; untimed warmup performed, ORM cache cold; open/rollback/setup/verification excluded",
            "floor_scope": "direct SQL via same transaction/driver, fixture-specific semantics; not a general ORM replacement",
        })
    );
    rt.block_on(fixture.harness.cleanup())?;
    Ok(())
}
