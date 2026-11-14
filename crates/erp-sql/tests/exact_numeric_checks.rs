// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use erp_fields::ErrorKind;
use erp_sql::exact_numeric::{carry_check, fraction_check, grid_check};

#[test]
fn definitions_are_quoted_guarded_and_deterministic() {
    let grid = grid_check("value", 18, false).unwrap();
    assert_eq!(grid, grid_check("value", 18, false).unwrap());
    assert!(grid.starts_with("CHECK (CASE WHEN"));
    assert!(grid.contains("\"value\""));
    assert!(grid.contains("min_scale"));
    assert!(
        grid_check("value", 18, true)
            .unwrap()
            .contains("IS NULL THEN TRUE")
    );
    assert!(
        carry_check("carry", "increment")
            .unwrap()
            .contains("greatest(18")
    );
    assert!(fraction_check("num", "den").unwrap().contains("THEN gcd("));
    let error = grid_check("value", 29, false).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "exact numeric grid scale must be between 0 and 28"
    );
    for bad in ["", "x\0y", "x; DROP TABLE t", "x.y"] {
        let error = fraction_check(bad, "den").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "exact numeric check requires a valid column identifier"
        );
    }
}

#[test]
fn canonical_sql_definitions_are_pinned_for_the_guard_slice() {
    let actual = [
        grid_check("value", 18, false).unwrap(),
        carry_check("carry", "increment").unwrap(),
        fraction_check("num", "den").unwrap(),
    ]
    .join("\n")
        + "\n";
    assert_eq!(
        actual,
        include_str!("fixtures/exact_numeric_checks.sql")
            .strip_prefix("-- SPDX-License-Identifier: MIT OR Apache-2.0\n")
            .unwrap()
    );
}
