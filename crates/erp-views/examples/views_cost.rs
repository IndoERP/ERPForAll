//! Fixed-iteration companion to views_combine, sharing its unchanged 41-view fixture.
#![allow(clippy::print_stdout, reason = "remote measurement producer")]
#[path = "../tests/support/partner_form.rs"]
mod partner_form;
use erp_views::ViewCatalogue;
use nix::sys::resource::{UsageWho, getrusage};
use std::{error::Error, hint::black_box, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let [kind, count, xml_path] = arguments.as_slice() else {
        return Err("usage: views_cost cold|combine|parse OPERATIONS OUTPUT.xml".into());
    };
    if !matches!(kind.as_str(), "cold" | "combine" | "parse") {
        return Err("kind must be cold, combine or parse".into());
    }
    let operations: usize = count.parse()?;
    if !(1..=10_000).contains(&operations) {
        return Err("operations must be 1..=10000".into());
    }
    let views = partner_form::fixture();
    if views.len() != 41 {
        return Err("expected unchanged 41-view fixture".into());
    }
    let retained_fixture_bytes: usize = views.iter().map(|view| view.arch().len()).sum();
    let selected = if kind == "parse" {
        vec![views.first().ok_or("base fixture absent")?.clone()]
    } else {
        views.clone()
    };
    let input_bytes: usize = selected.iter().map(|view| view.arch().len()).sum();
    let catalogue = ViewCatalogue::new(selected)?;
    let check = catalogue.combine(1)?;
    let expected_applied = if kind == "parse" { 1 } else { 41 };
    if check.applied_view_ids().len() != expected_applied {
        return Err("fixture did not apply the expected view count".into());
    }
    let xml = check.to_xml();
    std::fs::write(xml_path, &xml)?;
    println!(
        "case={kind}\nfixture_views={}\ninput_bytes={input_bytes}\nretained_fixture_bytes={retained_fixture_bytes}\noutput_bytes={}\napplied_ids={:?}",
        views.len(),
        xml.len(),
        check.applied_view_ids()
    );
    drop(check);
    drop(xml);
    let catalogue = if kind == "cold" {
        drop(catalogue);
        None
    } else {
        Some(catalogue)
    };

    let mut elapsed_ns = 0u128;
    // Cold clones are outside each timer, as the existing iter_batched setup is.
    // Cold result destruction is outside; catalogue destruction stays inside.
    // The existing combine/parse iter paths include returned-result destruction.
    for iteration in 0..operations + 8 {
        let elapsed = if kind == "cold" {
            let input = views.clone();
            let start = Instant::now();
            let result = {
                let catalogue = ViewCatalogue::new(input)?;
                black_box(catalogue.combine(black_box(1))?)
            };
            let elapsed = start.elapsed();
            drop(result);
            elapsed
        } else {
            let catalogue = catalogue.as_ref().ok_or("prepared catalogue missing")?;
            let start = Instant::now();
            drop(black_box(catalogue.combine(black_box(1))?));
            start.elapsed()
        };
        if iteration >= 8 {
            elapsed_ns = elapsed_ns
                .checked_add(elapsed.as_nanos())
                .ok_or("elapsed time overflow")?;
        }
    }
    let raw_peak = u64::try_from(getrusage(UsageWho::RUSAGE_SELF)?.max_rss())?;
    if raw_peak == 0 {
        return Err("running process reported zero resident peak".into());
    }
    #[cfg(target_os = "macos")]
    let peak_bytes = raw_peak;
    #[cfg(target_os = "linux")]
    let peak_bytes = raw_peak.checked_mul(1024).ok_or("resident peak overflow")?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err("resident peak units supported only on macOS and Linux".into());
    println!(
        "operations={operations}\nwarmups=8\nelapsed_nanoseconds={elapsed_ns}\nresident_peak_bytes={peak_bytes}\nresident_peak_source=getrusage(RUSAGE_SELF).ru_maxrss through completed workload"
    );
    Ok(())
}
