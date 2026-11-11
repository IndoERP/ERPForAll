//! Envelope measurement of the real sanitiser: for every
//! entry of the adversarial corpus, one `erp_html::sanitize` call under `Limits::ceiling()` with the
//! heap peak dhat observed around the call, and the charged peak found by searching for the
//! smallest working-memory limit under which the call still succeeds (the refusal fires when a
//! charge would pass the limit, so that limit is the call's highest live charge). Prints one JSON
//! line per entry with the keys `scripts/html-budget-envelope.sh` summarises: `charged_peak` is 0
//! for an entry the sanitiser refused with a typed limit error. An internal fault or a refusal that
//! is not a limit is a panic, never a row. Run only on the build host
//! (`scripts/html-budget-envelope.sh --sink erp-html`); product latency is not measured.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "measurement harness"
)]

#[path = "../../erp-ui-compiler/tests/html_adversarial/mod.rs"]
mod html_adversarial;

use erp_alloc_budget as alloc;
use erp_html::{DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_empty,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_attribute_floods,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_nesting,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_char_ref_bombs,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_unclosed,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    envelope_multibyte,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    ratchet_heap_peak_follows_the_committed_fit,
    admission_recommended_limits_end_ok_or_typed_refusal,
);

fn policy() -> SanitizePolicy {
    SanitizePolicy::Default(DefaultPolicy::new())
}

fn run(input: &str, limits: &Limits) -> Result<(), HtmlSanitizeError> {
    sanitize(input, &policy(), limits, SanitizeMode::Clean).map(drop)
}

/// Short name of a typed limit refusal; any other error is a bug the harness must not hide.
fn refusal_name(entry: &str, error: &HtmlSanitizeError) -> &'static str {
    match error {
        HtmlSanitizeError::InputTooLarge { .. } => "InputTooLarge",
        HtmlSanitizeError::DepthExceeded { .. } => "DepthExceeded",
        HtmlSanitizeError::NodeLimit { .. } => "NodeLimit",
        HtmlSanitizeError::WorkLimit { .. } => "WorkLimit",
        HtmlSanitizeError::MemoryLimit { .. } => "MemoryLimit",
        HtmlSanitizeError::OutputTooLarge { .. } => "OutputTooLarge",
        other => panic!("{entry}: not a typed limit refusal: {other}"),
    }
}

/// Smallest working-memory limit under which `input` still succeeds, the call's charged peak. The
/// call must already succeed at the ceiling.
fn charged_peak(entry: &str, input: &str) -> u64 {
    let ceiling = Limits::ceiling().working_bytes();
    let (mut low, mut high) = (1, ceiling);
    while low < high {
        let mid = low + (high - low) / 2;
        let limits = Limits::ceiling().with_working_bytes(mid).unwrap();
        match run(input, &limits) {
            Ok(()) => high = mid,
            Err(HtmlSanitizeError::MemoryLimit { .. }) => low = mid + 1,
            Err(other) => panic!("{entry}: at working limit {mid}: {other}"),
        }
    }
    low
}

fn row(family: &str, entry: &str, input: &str) {
    let limits = Limits::ceiling();
    let (stats, result) = alloc::measure(|| run(input, &limits)).unwrap();
    let (charged, outcome) = match result {
        Ok(()) => (charged_peak(entry, input), "parsed".to_owned()),
        Err(error) => (0, format!("refused:{}", refusal_name(entry, &error))),
    };
    println!(
        "{{\"family\":\"{family}\",\"entry\":\"{entry}\",\"input_bytes\":{},\"charged_peak\":{charged},\"peak_bytes\":{},\"total_bytes\":{},\"outcome\":\"{outcome}\"}}",
        input.len(),
        stats.peak_bytes,
        stats.total_bytes,
    );
}

/// The entries of one corpus family as `(name, input)`.
fn family_entries(family: &str) -> Vec<(String, String)> {
    html_adversarial::all()
        .into_iter()
        .find(|(f, _)| *f == family)
        .unwrap_or_else(|| panic!("unknown corpus family {family}"))
        .1
        .into_iter()
        .map(|e| (e.name, e.chunks.concat()))
        .collect()
}

/// Every corpus entry as `(name, input)`.
fn corpus() -> Vec<(String, String)> {
    html_adversarial::all()
        .into_iter()
        .flat_map(|(_, entries)| entries)
        .map(|e| (e.name, e.chunks.concat()))
        .collect()
}

fn run_family(family: &str) {
    for (name, input) in family_entries(family) {
        row(family, &name, &input);
    }
}

/// The raw rows of the committed envelope log that `limits.rs` names.
fn committed_rows() -> Vec<serde_json::Value> {
    let source = include_str!("../../erp-html/src/limits.rs");
    let path = source
        .lines()
        .find_map(|line| line.strip_prefix("// ENVELOPE-LOG: "))
        .expect("limits.rs names its envelope log");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join(path.trim())).expect("the named log is committed");
    let mut rows = Vec::new();
    let mut in_rows = false;
    for line in text.lines() {
        match line {
            "# raw rows" => in_rows = true,
            "# summary" => in_rows = false,
            _ if in_rows => rows.push(serde_json::from_str::<serde_json::Value>(line).unwrap()),
            _ => {}
        }
    }
    rows
}

/// `C` and `K_input = num / den` of the committed rows: the heap peak of the empty entry and the
/// largest `(peak - C) / input` over the entries that parsed.
fn committed_fit(rows: &[serde_json::Value]) -> (u128, u128, u128) {
    let parsed: Vec<_> = rows.iter().filter(|r| r["outcome"] == "parsed").collect();
    let field = |r: &serde_json::Value, key: &str| u128::from(r[key].as_u64().unwrap());
    let c = field(
        parsed
            .iter()
            .find(|r| r["entry"] == "empty")
            .expect("the log has a parsed empty entry"),
        "peak_bytes",
    );
    let (mut num, mut den) = (0, 1);
    for r in parsed.iter().filter(|r| field(r, "input_bytes") > 0) {
        let peak = field(r, "peak_bytes");
        let n = peak.checked_sub(c).unwrap_or_else(|| {
            panic!("committed row {}: peak {peak} is below C = {c}", r["entry"])
        });
        let d = field(r, "input_bytes");
        if n * den > num * d {
            (num, den) = (n, d);
        }
    }
    (c, num, den)
}

/// A fresh measurement under the recommended limits stays inside the fit of the committed log.
fn ratchet_heap_peak_follows_the_committed_fit() {
    let rows = committed_rows();
    let (c, num, den) = committed_fit(&rows);
    let limits = Limits::recommended();
    for (name, input) in corpus() {
        let (stats, result) = alloc::measure(|| run(&input, &limits)).unwrap();
        if let Err(error) = result {
            // A refusal is expected only for an entry the committed log did not parse, or one
            // above the recommended input cap; a parsed entry turning into a refusal is a regression.
            let was_parsed = rows
                .iter()
                .any(|r| r["entry"] == name.as_str() && r["outcome"] == "parsed");
            assert!(
                !was_parsed || input.len() as u64 > limits.input_bytes(),
                "{name}: parsed in the committed log, now refused: {error}"
            );
            continue;
        }
        let (peak, len) = (u128::from(stats.peak_bytes), input.len() as u128);
        assert!(
            peak * den <= num * len + c * den,
            "{name}: heap peak {peak} for {len} input bytes is above the committed fit \
             (C = {c}, K_input = {num}/{den})"
        );
    }
}

/// At the recommended limits every entry ends `Ok` or in a typed limit refusal.
fn admission_recommended_limits_end_ok_or_typed_refusal() {
    let limits = Limits::recommended();
    for (name, input) in corpus() {
        if let Err(error) = run(&input, &limits) {
            refusal_name(&name, &error);
        }
    }
}

fn envelope_empty() {
    run_family("empty");
}
fn envelope_attribute_floods() {
    run_family("attribute_floods");
}
fn envelope_nesting() {
    run_family("nesting");
}
fn envelope_char_ref_bombs() {
    run_family("char_ref_bombs");
}
fn envelope_unclosed() {
    run_family("unclosed");
}
fn envelope_multibyte() {
    run_family("multibyte");
}
