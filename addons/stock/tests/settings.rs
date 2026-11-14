// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native inventory settings contracts; factories are discovered exactly as installed composition does.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_sdk::settings::{
    CONTRIBUTIONS, GROUP_CONTRIBUTIONS, ScopeKind, SettingDecl, SettingsRegistry,
};
use erp_sdk::{ErrorKind, Value};
use std::collections::{BTreeMap, BTreeSet};

fn declarations() -> Vec<SettingDecl> {
    let addon = &erp_addon_stock::ADDON;
    let factories: Vec<_> = CONTRIBUTIONS
        .iter()
        .filter(|c| c.addon.name == addon.name)
        .collect();
    assert_eq!(
        factories.len(),
        1,
        "stock scalar registration must be linked"
    );
    (factories[0].declarations)().unwrap()
}
fn registry() -> SettingsRegistry {
    let installed = BTreeSet::from(["stock".into(), "base".into()]);
    let groups: Vec<_> = GROUP_CONTRIBUTIONS
        .iter()
        .filter(|c| c.addon.name == erp_addon_stock::ADDON.name)
        .collect();
    assert_eq!(groups.len(), 1, "stock annual-date group must be linked");
    SettingsRegistry::build(vec![("stock".into(), declarations())], &installed, |_| true)
        .unwrap()
        .with_groups(
            vec![("stock".into(), (groups[0].declarations)().unwrap())],
            &installed,
        )
        .unwrap()
}

#[test]
fn scalar_domains_are_typed_bounded_and_named() {
    let declarations = declarations();
    assert_eq!(declarations.len(), 8);
    let cases = [
        (
            "stock.horizon_days",
            Value::Int(0),
            "stock.horizon_days takes a whole number from 1 to 3650.",
        ),
        (
            "stock.horizon_days",
            Value::Int(3651),
            "stock.horizon_days takes a whole number from 1 to 3650.",
        ),
        (
            "stock.horizon_days",
            Value::Bool(true),
            "stock.horizon_days takes a whole number from 1 to 3650.",
        ),
        (
            "stock.annual_inventory_day",
            Value::Int(32),
            "stock.annual_inventory_day takes a whole number from 1 to 31.",
        ),
        (
            "stock.picking_policy",
            Value::str("unknown"),
            "stock.picking_policy takes one of direct, one.",
        ),
        (
            "stock.annual_inventory_month",
            Value::str("0"),
            "stock.annual_inventory_month takes one of 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12.",
        ),
        (
            "stock.stock_move_email_validation",
            Value::Int(1),
            "stock.stock_move_email_validation takes true or false.",
        ),
        (
            "stock.barcode_separator",
            Value::str(""),
            "stock.barcode_separator takes 1 to 16 UTF-8 bytes without control characters.",
        ),
        (
            "stock.barcode_separator",
            Value::str("\n"),
            "stock.barcode_separator takes 1 to 16 UTF-8 bytes without control characters.",
        ),
        (
            "stock.barcode_separator",
            Value::str("abcdefghijklmnopq"),
            "stock.barcode_separator takes a text of at most 16 bytes.",
        ),
    ];
    for (key, value, expected) in cases {
        let declaration = declarations.iter().find(|d| d.key() == key).unwrap();
        let error = declaration.validate(&value).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), expected);
    }
    for (key, value) in [
        ("stock.horizon_days", Value::Int(1)),
        ("stock.horizon_days", Value::Int(3650)),
        ("stock.barcode_separator", Value::str("abcdefghijklmnop")),
        ("stock.barcode_separator", Value::str("éééééééé")),
        ("stock.stock_confirmation_type", Value::str("sms")),
    ] {
        declarations
            .iter()
            .find(|d| d.key() == key)
            .unwrap()
            .validate(&value)
            .unwrap();
    }
    assert_eq!(
        declarations
            .iter()
            .filter(|d| d.scope() == ScopeKind::Company)
            .count(),
        7
    );
}

#[test]
fn annual_date_group_refuses_impossible_dates_without_clamping() {
    let registry = registry();
    let expanded = registry
        .expanded_keys(&["stock.annual_inventory_month".into()])
        .unwrap();
    assert_eq!(
        expanded,
        vec!["stock.annual_inventory_day", "stock.annual_inventory_month"]
    );
    for month in 1..=12 {
        let max = match month {
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let mut values = BTreeMap::from([
            (
                "stock.annual_inventory_month".into(),
                Value::str(month.to_string()),
            ),
            ("stock.annual_inventory_day".into(), Value::Int(max)),
        ]);
        registry.validate_groups(&values).unwrap();
        values.insert("stock.annual_inventory_day".into(), Value::Int(max + 1));
        let error = registry.validate_groups(&values).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!(
                "stock.annual_inventory_day takes 1 to {max} for stock.annual_inventory_month {month}."
            )
        );
    }
}
