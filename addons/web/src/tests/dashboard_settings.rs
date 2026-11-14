// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use super::*;
#[test]
fn dashboard_period_choices_are_bounded_and_strict() {
    assert_eq!(
        parse(DEFAULT_PERIODS)
            .unwrap()
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        [
            "last_7_days",
            "last_30_days",
            "last_90_days",
            "this_month",
            "this_year"
        ]
    );
    for text in [
        "[]",
        "{}",
        r#"[{"id":"a","label":"A","kind":"rolling","days":0}]"#,
        r#"[{"id":"a","label":"A","kind":"month","days":3}]"#,
        r#"[{"id":"a","label":"A","kind":"month","extra":1}]"#,
        r#"[{"id":"a","label":"A","kind":"month"},{"id":"a","label":"B","kind":"year"}]"#,
    ] {
        let error = parse(text).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), invalid().message());
    }
}
#[test]
fn dashboard_settings_refuse_zero_and_above_bound() {
    let declarations = declarations().unwrap();
    for (decl, upper) in declarations.iter().take(2).zip([32, 3660]) {
        for value in [0, upper + 1] {
            let error = decl.validate(&Value::Int(value)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!("{} takes {}.", decl.key(), decl.kind().describe())
            );
        }
    }
}

#[test]
fn dashboard_settings_refuse_periods_beyond_the_horizon() {
    for (maximum, periods) in [
        (6, r#"[{"id":"a","label":"A","kind":"rolling","days":7}]"#),
        (30, r#"[{"id":"a","label":"A","kind":"month"}]"#),
        (365, r#"[{"id":"a","label":"A","kind":"year"}]"#),
    ] {
        let values = BTreeMap::from([
            (DAYS.to_owned(), Value::Int(maximum)),
            (PERIODS.to_owned(), Value::Str(periods.into())),
        ]);
        let error = validate_group(&values).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "web.dashboard_periods contains a period beyond web.dashboard_max_period_days."
        );
    }
}
