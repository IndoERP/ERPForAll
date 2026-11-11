//! Names a package declares in `describe()` are validated before anything keeps them (security
//! review r1 M3).

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use erp_wasm_host::{HostError, MAX_DESCRIBED_METHODS, described_methods};

fn describe(model: &str, name: &str) -> String {
    serde_json::json!({
        "schema": 1,
        "contributions": [{"addon": "a", "kind": "method", "model": model, "name": name, "detail": {"args": []}}],
    })
    .to_string()
}

#[test]
fn valid_names_are_accepted() {
    let got = described_methods("a", &describe("sale.order.line", "action_confirm2"))
        .expect("valid names");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].model, "sale.order.line");
}

#[test]
fn invalid_names_are_refused() {
    let long = "a".repeat(129);
    for (model, name) in [
        ("sale.order", "bad name"),
        ("sale.order", "a.b"),
        ("sale.order", ""),
        ("sale.order", long.as_str()),
        ("Sale.Order", "ok"),
        ("sale order", "ok"),
        ("", "ok"),
        (long.as_str(), "ok"),
        ("sale.order", "naïve"),
    ] {
        let err = described_methods("a", &describe(model, name)).expect_err(name);
        assert!(matches!(err, HostError::Config(_)), "{err:?}");
    }
}

#[test]
fn other_addons_contributions_are_skipped() {
    let text = serde_json::json!({
        "schema": 1,
        "contributions": [{"addon": "other", "kind": "method", "model": "m", "name": "x y"}],
    })
    .to_string();
    assert!(described_methods("a", &text).expect("skipped").is_empty());
}

fn many(count: usize) -> String {
    let list: Vec<_> = (0..count)
        .map(|i| {
            serde_json::json!({"addon": "a", "kind": "method", "model": "m.x", "name": format!("m_{i}"), "detail": {"args": []}})
        })
        .collect();
    serde_json::json!({"schema": 1, "contributions": list}).to_string()
}

#[test]
fn a_package_over_the_method_cap_is_refused_and_later_reads_still_work() {
    let at = described_methods("a", &many(MAX_DESCRIBED_METHODS)).expect("at the bound");
    assert_eq!(at.len(), MAX_DESCRIBED_METHODS);
    let err = described_methods("a", &many(MAX_DESCRIBED_METHODS + 1)).expect_err("over the cap");
    let HostError::Config(message) = &err else {
        panic!("not a Config error: {err:?}")
    };
    assert_eq!(
        message,
        &format!(
            "describe() of 'a': more than {MAX_DESCRIBED_METHODS} method contributions; refusing the package"
        )
    );
    // another addon's contributions do not count against this one
    let other = serde_json::json!({
        "schema": 1,
        "contributions": (0..=MAX_DESCRIBED_METHODS)
            .map(|i| serde_json::json!({"addon": "other", "kind": "method", "model": "m.x", "name": format!("o_{i}")}))
            .collect::<Vec<_>>(),
    })
    .to_string();
    assert!(described_methods("a", &other).expect("not a's").is_empty());
    assert_eq!(
        described_methods("a", &describe("sale.order", "ok_method"))
            .expect("later reads work")
            .len(),
        1
    );
}
