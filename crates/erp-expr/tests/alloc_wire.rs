//! Encoder allocation measurements and ratchets for.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_alloc_budget as alloc;
use erp_expr::{Expr, Literal, wire};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    encode_scalar,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    encode_list,
    encode_scalar_semantics,
    encode_list_semantics,
    source_preflight_success_allocates_nothing,
    source_reservation_covers_parse_and_admission,
);

fn measure_expression(name: &str, expression: &Expr, nodes: u64, blocks: u64, peak: u64) {
    let (stats, encoded) = alloc::measure(|| wire::encode(expression)).unwrap();
    let encoded = encoded.unwrap();
    println!(
        "wire {name}: nodes={nodes} encoded_bytes={} stats={stats:?}",
        encoded.len()
    );
    // Keep the design
    // targets as well as the exact measured ratchets; neither permits added slack.
    assert!(stats.total_blocks <= 4 + 2 * nodes, "{stats:?}");
    assert!(
        stats.peak_bytes <= 2 * encoded.len() as u64 + 32 * nodes,
        "{stats:?}"
    );
    assert!(stats.total_blocks <= blocks, "{stats:?}");
    assert!(stats.peak_bytes <= peak, "{stats:?}");
    assert_eq!(stats.end_blocks, 1);
    assert_eq!(stats.end_bytes, encoded.len() as u64);
}

fn encode_scalar() {
    measure_expression("scalar", &Expr::Literal(Literal::Int(i64::MAX)), 1, 4, 169);
}

fn encode_list() {
    let expression = Expr::List((0..1000).map(|n| Expr::Literal(Literal::Int(n))).collect());
    measure_expression("list", &expression, 1001, 1021, 140268);
}

fn encode_scalar_semantics() {
    let expression = Expr::Literal(Literal::Int(i64::MAX));
    let encoded = wire::encode(&expression).unwrap();
    assert_eq!(
        wire::ValidatedAst::from_json(&encoded).unwrap().as_expr(),
        &expression
    );
}

fn encode_list_semantics() {
    let expression = Expr::List((0..1000).map(|n| Expr::Literal(Literal::Int(n))).collect());
    let encoded = wire::encode(&expression).unwrap();
    assert_eq!(
        wire::ValidatedAst::from_json(&encoded).unwrap().as_expr(),
        &expression
    );
}

fn source_corpus() -> Vec<(&'static str, String)> {
    vec![
        (
            "domain",
            "[('name', '=', 'x'), ('qty', 'in', [1, 2, 3])]".to_owned(),
        ),
        ("context", "{'group_by': ['date:month', 'name']}".to_owned()),
        (
            "lookup and calls",
            "context.get('key', 1) if active else value[1:3]".to_owned(),
        ),
        ("adjacent strings", "'one' 'two' 'three'".to_owned()),
        ("formatted name", "f'outer {name}'".to_owned()),
        ("decoded replacement", "f'\\x7bname\\x7d'".to_owned()),
        (
            "nested formatted name",
            "f'outer {f\"inner {name}\"}'".to_owned(),
        ),
        ("raw formatted literal", "rf'\\x7bname\\x7d'".to_owned()),
        (
            "comprehension",
            "[a+b for a in x for b in y if b]".to_owned(),
        ),
        (
            "wide integer and float",
            "[-9223372036854775807, 0.1, 1.5]".to_owned(),
        ),
        ("non-ASCII", "'é😀\\n'".to_owned()),
        (
            "source ceiling literal",
            format!("'{}'", "x".repeat(64 * 1024 - 2)),
        ),
        (
            "4096 values",
            format!(
                "[{}]",
                (0..4096)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "large formatted literal prefix",
            format!("f'{}{{name}}'", "!".repeat(60 * 1024)),
        ),
    ]
}

fn source_preflight_success_allocates_nothing() {
    for (name, source) in source_corpus() {
        let (stats, bound) =
            alloc::measure(|| erp_expr::source_preflight(&source, erp_expr::EvalLimits::default()))
                .unwrap();
        bound.expect("source preflight succeeds");
        assert_eq!(stats.total_blocks, 0, "{name}: {stats:?}");
        assert_eq!(stats.total_bytes, 0, "{name}: {stats:?}");
    }
}

fn source_reservation_covers_parse_and_admission() {
    for (name, source) in source_corpus() {
        let limits = erp_expr::EvalLimits::default();
        let bound = erp_expr::source_preflight(&source, limits).unwrap();
        let (stats, result) = alloc::measure(|| {
            erp_expr::parse_with_limits(&source, limits).map(|parsed| {
                let admitted = wire::ValidatedAst::from_expr(&parsed).expect("wire admission");
                (parsed, admitted)
            })
        })
        .unwrap();
        let trees = result.expect("source parses");
        println!(
            "{name}: source_bytes={} reserve={} tokens={} depth={} peak={} end={} total_blocks={} fits_32m={}",
            source.len(),
            bound.reserve_bytes(),
            bound.token_bound(),
            bound.depth_bound(),
            stats.peak_bytes,
            stats.end_bytes,
            stats.total_blocks,
            bound.reserve_bytes() <= 32 * 1024 * 1024
        );
        assert!(
            stats.peak_bytes <= bound.reserve_bytes() as u64,
            "{name}: peak {} exceeds source reservation {}",
            stats.peak_bytes,
            bound.reserve_bytes()
        );
        drop(trees);
    }
}
