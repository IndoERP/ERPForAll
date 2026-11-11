//! Bounded timing and resident-peak producer for the HTTP/RPC and SQL-floor fixture.
//! One mode/operation per fresh process, only in a reserved quiet Linux window.
//! No allocation profiler is activated. Retained setup memory remains in the process peak.
#![allow(clippy::print_stdout, reason = "diagnostic producer")]
#[path = "../tests/support/copy_serve.rs"]
mod copy_serve;

use copy_serve::{Mode, OPERATIONS, Operation};
use std::{error::Error, time::Instant};

// Same Linux kernel interface as erp-bench/src/memory.rs. Refuse unsupported hosts;
// a cumulative startup peak must never masquerade as a request-window peak.
fn reset_peak() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "linux") {
        return Err("copy resident-peak measurement requires Linux clear_refs=5".into());
    }
    std::fs::write("/proc/self/clear_refs", "5")?;
    Ok(())
}

fn resident_peak_bytes() -> Result<u64, Box<dyn Error>> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or("missing VmHWM in /proc/self/status")?
        .trim()
        .strip_suffix("kB")
        .ok_or("VmHWM unit must be kB")?
        .trim()
        .parse()?;
    Ok(kib.checked_mul(1024).ok_or("VmHWM bytes overflow")?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err("usage: copy_serve_probe <1|1000> <erp|floor> <RPC method>".into());
    }
    let rows: usize = args[0].parse()?;
    if rows != 1 && rows != 1000 {
        return Err("rows must be 1 or 1000".into());
    }
    let mode = match args[1].as_str() {
        "erp" => Mode::Erp,
        "floor" => Mode::Floor,
        _ => return Err("mode must be erp or floor".into()),
    };
    let operation = OPERATIONS
        .into_iter()
        .find(|operation| operation.method() == args[2])
        .ok_or("unsupported RPC method")?;
    if operation == Operation::Name && rows != 1 {
        return Err("name_create fixture supports only one row".into());
    }
    reset_peak()?; // Fail before starting a fixture on an unsupported host.
    println!(
        "COPY-SERVED-PROCESS pid={} mode={mode:?} operation={operation:?} rows={rows} allocator=normal",
        std::process::id()
    );
    let server = copy_serve::start(mode, operation, rows)?;
    let mut client = server.client()?;
    let expected = operation.expected(rows);
    let mut samples = Vec::with_capacity(64);
    let mut max_peak_bytes = 0;
    let mut expected_state = None;
    for sample in 0..72 {
        server.reset(&mut client)?;
        server.probe.settle()?;
        // Reset after fixture/control work. The peak still includes retained setup,
        // client, server, allocator arenas and this normal-allocator process's runtime.
        reset_peak()?;
        let start = Instant::now();
        let response = client.send()?;
        let elapsed = start.elapsed();
        server.probe.settle()?;
        let peak_bytes = resident_peak_bytes()?;
        if response.status() != 200 {
            return Err(format!("unexpected response status {}", response.status()).into());
        }
        let result: serde_json::Value =
            serde_json::from_slice(response.content().ok_or("missing JSON response")?)?;
        if result != expected {
            return Err("response semantics differ from fixture".into());
        }
        let state = server.state(&mut client)?;
        if let Some(expected) = &expected_state {
            if &state != expected {
                return Err("database effect differs across repetitions".into());
            }
        } else {
            expected_state = Some(state);
        }
        if sample >= 8 {
            let ns = u64::try_from(elapsed.as_nanos())?;
            samples.push(ns);
            max_peak_bytes = max_peak_bytes.max(peak_bytes);
            println!(
                "COPY-SERVED-TIME mode={mode:?} operation={operation:?} rows={rows} sample={sample} elapsed_ns={ns} resident_peak_bytes={peak_bytes}"
            );
        }
    }
    samples.sort_unstable();
    println!(
        "COPY-SERVED-PERCENTILES mode={mode:?} operation={operation:?} rows={rows} samples={} p50_ns={} sample_max_ns={} resident_peak_bytes={max_peak_bytes} source=VmHWM-reset-per-request",
        samples.len(),
        samples[31],
        samples[63]
    );
    // The shell producer compares these actual durable effects across fresh ERP/floor
    // processes. Printing/semantic checks happen outside both measurement windows.
    println!(
        "COPY-SERVED-STATE {}",
        expected_state.ok_or("no database state observed")?
    );
    drop(client);
    server.stop()?;
    Ok(())
}
