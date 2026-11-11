//! Pure source admission describes a conservative bound before parse/admission allocation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_expr::{EvalLimits, ExprError, parse_with_limits, source_preflight, wire::WireDocument};

#[test]
fn profile_bounds_the_wire_nodes_and_preserves_source_limit_refusals() {
    for source in [
        "name + 1",
        "[('name', '=', 'x'), ('qty', 'in', [1, 2, 3])]",
        "{'group_by': ['date:month', 'name']}",
        "f'outer {name}'",
        "f'\\x7bname\\x7d'",
        "f'outer {f\"inner {name}\"}'",
    ] {
        let limits = EvalLimits::default();
        let profile = source_preflight(source, limits).unwrap();
        let parsed = parse_with_limits(source, limits).unwrap();
        let wire = WireDocument::from_expr(&parsed).unwrap();
        assert!(profile.token_bound() >= wire.nodes.len(), "{source}");
        assert_eq!(profile.depth_bound(), limits.max_depth());
        assert!(profile.reserve_bytes() > 0);
    }
    let error =
        source_preflight("abcde", EvalLimits::default().with_max_source_bytes(4)).unwrap_err();
    assert_eq!(
        error,
        ExprError::LimitExceeded {
            message: "expression source of 5 bytes exceeds the limit of 4 bytes".to_owned(),
        }
    );
    assert_eq!(
        error.to_string(),
        "LimitExceeded: expression source of 5 bytes exceeds the limit of 4 bytes"
    );
}

#[test]
fn exact_source_ceiling_literal_has_a_usable_construction_bound() {
    let source = format!("'{}'", "x".repeat(64 * 1024 - 2));
    assert_eq!(source.len(), EvalLimits::default().max_source_bytes());
    let profile = source_preflight(&source, EvalLimits::default()).unwrap();
    assert!(profile.reserve_bytes() <= 32 * 1024 * 1024);
    let error = source_preflight(&(source + " "), EvalLimits::default()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "LimitExceeded: expression source of 65537 bytes exceeds the limit of 65536 bytes"
    );
    assert!(matches!(error, ExprError::LimitExceeded { .. }));
}
