//! Bounded normal-allocator measurement of complete AST byte admission and result drop.
#![allow(clippy::print_stdout, reason = "remote measurement producer")]
use erp_expr::{Expr, Literal, wire};
use nix::sys::resource::{UsageWho, getrusage};
use std::{error::Error, hint::black_box, time::Instant};

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let [case, count, output_path] = arguments.as_slice() else {
        return Err("usage: admission_cost scalar|list|alternate OPERATIONS OUTPUT.json".into());
    };
    let operations: usize = count.parse()?;
    if !(1..=2_000_000).contains(&operations) {
        return Err("operations must be 1..=2000000".into());
    }
    let (expression, nodes) = match case.as_str() {
        "scalar" => (Expr::Literal(Literal::Int(i64::MAX)), 1),
        "list" | "alternate" => (
            Expr::List((0..1000).map(|n| Expr::Literal(Literal::Int(n))).collect()),
            1001,
        ),
        _ => return Err("unknown fixture".into()),
    };
    let canonical = wire::encode(&expression)?;
    let input = if case == "alternate" {
        let old = format!(
            "\"items\":[{}]",
            (0..1000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let new = format!(
            "\"items\":[{}]",
            (0..1000)
                .map(|n| format!("{n}e0"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let raw = std::str::from_utf8(&canonical)?;
        if raw.matches(&old).count() != 1 {
            return Err("expected one actual list-index array".into());
        }
        raw.replace(&old, &new).into_bytes()
    } else {
        canonical.clone()
    };
    let admitted = wire::ValidatedAst::from_json(&input)?;
    if admitted.as_expr() != &expression {
        return Err("admitted fixture changed expression semantics".into());
    }
    let encoded = admitted.to_json()?;
    if encoded != canonical {
        return Err("admitted fixture changed canonical bytes".into());
    }
    std::fs::write(output_path, &encoded)?;
    drop(admitted);
    drop(encoded);
    let mut elapsed = 0u128;
    for iteration in 0..operations + 8 {
        let start = Instant::now();
        drop(black_box(wire::ValidatedAst::from_json(black_box(&input))?));
        let duration = start.elapsed();
        if iteration >= 8 {
            elapsed = elapsed
                .checked_add(duration.as_nanos())
                .ok_or("time overflow")?;
        }
    }
    let raw_peak = u64::try_from(getrusage(UsageWho::RUSAGE_SELF)?.max_rss())?;
    if raw_peak == 0 {
        return Err("zero resident peak".into());
    }
    #[cfg(target_os = "macos")]
    let peak = raw_peak;
    #[cfg(target_os = "linux")]
    let peak = raw_peak.checked_mul(1024).ok_or("resident peak overflow")?;
    println!(
        "case={case}\nfixture_nodes={nodes}\ninput_bytes={}\noutput_bytes={}\noperations={operations}\nwarmups=8\nelapsed_nanoseconds={elapsed}\nresident_peak_bytes={peak}\nresident_peak_source=getrusage(RUSAGE_SELF).ru_maxrss through completed workload",
        input.len(),
        canonical.len()
    );
    Ok(())
}
