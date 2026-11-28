// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use serde_json::json;

fn valid_panel() -> Json {
    json!({"title":"Orders by stage","model":"sale.order","groupby":["state"],"aggregates":["id:count"],"order":"","domain":[],"limit":10})
}

#[test]
fn stored_definitions_reject_embedded_numbers_and_override_contexts() {
    let panel = valid_panel();
    assert!(parse(&json!({"panels":[panel]}).to_string()).is_ok());
    for key in ["context", "rows", "data", "html", "url"] {
        let mut panel = valid_panel();
        panel[key] = json!("test");
        assert!(
            parse(&json!({"panels":[panel]}).to_string()).is_err(),
            "{key}"
        );
    }
    assert!(parse(&json!({"panels":[]}).to_string()).is_err());
    assert!(parse(&json!({"panels":vec![panel.clone();9]}).to_string()).is_err());
    assert!(parse(&json!({"report_id":0}).to_string()).is_err());
    assert!(parse(&json!({"report_id":1,"panels":[panel]}).to_string()).is_err());
    assert!(parse(&json!({"report_id":1}).to_string()).is_ok());
}

#[test]
fn mixed_currency_aggregates_require_a_currency_axis_or_filter() {
    let mut panel: Panel = serde_json::from_value(valid_panel()).unwrap();
    assert!(!crate::aggregate::single_currency(
        &panel.groupby,
        &panel.domain,
        "currency_id"
    ));
    panel.domain = vec![json!(["currency_id", "!=", 1])];
    assert!(!crate::aggregate::single_currency(
        &panel.groupby,
        &panel.domain,
        "currency_id"
    ));
    panel.domain = vec![json!(["currency_id", "=", 1])];
    assert!(crate::aggregate::single_currency(
        &panel.groupby,
        &panel.domain,
        "currency_id"
    ));
    panel.domain.clear();
    panel.groupby = vec!["currency_id".to_owned()];
    assert!(crate::aggregate::single_currency(
        &panel.groupby,
        &panel.domain,
        "currency_id"
    ));
}
