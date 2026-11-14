// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use super::*;
#[test]
fn dashboard_exact_comparisons_handle_zero_and_negative_baselines() {
    assert_eq!(
        gain(Some(Decimal::from(15)), Some(Decimal::from(10))).unwrap(),
        Some("50.00".to_owned())
    );
    assert_eq!(
        gain(Some(Decimal::from(-5)), Some(Decimal::from(-10))).unwrap(),
        Some("50.00".to_owned())
    );
    assert_eq!(gain(Some(Decimal::ONE), Some(Decimal::ZERO)).unwrap(), None);
    assert_eq!(gain(None, Some(Decimal::ONE)).unwrap(), None);
}
#[test]
fn dashboard_domains_refuse_execution() {
    let expr = erp_sdk::expr::parse("context.get('secret')").unwrap();
    let error = literal(&expr, 7, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Dashboard domains support literal criteria and uid only."
    );
}

#[test]
fn dashboard_domains_admit_signed_literals_and_uid() {
    let expr = erp_sdk::expr::parse("[('amount', '>=', -2.5), ('owner', '=', uid)]").unwrap();
    assert_eq!(
        literal(&expr, 7, 0).unwrap(),
        json!([["amount", ">=", -2.5], ["owner", "=", 7]])
    );
}

#[test]
fn dashboard_named_filters_refuse_ambiguous_identity() {
    let node = ViewNode::SearchFilter {
        name: "open".into(),
        label: "Open".into(),
        domain: None,
        context: None,
    };
    let error = named_filter([node.clone(), node].iter(), "open").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Dashboard named filter 'open' is ambiguous."
    );
}
