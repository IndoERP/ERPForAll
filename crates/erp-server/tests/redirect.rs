#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::redirect::RedirectPolicy;

#[test]
fn shell_url_contract_corpus() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("../../../testdata/shell-url-contract.json")).unwrap();
    assert_eq!(corpus["schema"], 1);
    let policy = RedirectPolicy::new("https://erp.example.test", &["list", "form"]).unwrap();
    for case in corpus["cases"].as_array().unwrap() {
        let result = policy.shell(case["input"].as_str().unwrap());
        if let Some(expected) = case["canonical"].as_str() {
            assert_eq!(result.unwrap(), expected, "{}", case["name"]);
        } else {
            let error = result.expect_err(case["name"].as_str().unwrap());
            assert_eq!(
                error,
                erp_server::boundary::BoundaryError::Redirect,
                "{}",
                case["name"]
            );
            assert_eq!(
                error.to_string(),
                "redirect must be an allowed local shell URL",
                "{}",
                case["name"]
            );
        }
    }
}
#[test]
fn redirects_are_same_origin_but_shell_state_has_its_own_closed_grammar() {
    let policy =
        RedirectPolicy::new("https://erp.example.test", &["list", "form", "addon_view"]).unwrap();
    for (input, expected) in [
        ("/web/login_successful", "/web/login_successful"),
        ("https://erp.example.test/odoo/action-42", "/odoo/action-42"),
        ("/odoo", "/odoo"),
    ] {
        assert_eq!(policy.normalize(input).unwrap(), expected);
    }
    for (input, expected) in [
        ("/web", "/erp"),
        ("/odoo?action=42", "/erp/action-42"),
        (
            "/web?action=42&view_type=addon_view",
            "/erp/action-42?view_type=addon_view",
        ),
        (
            "/erp/action-42?cids=1%2C2&view_type=form&id=%34&menu=7",
            "/erp/action-42?menu=7&id=4&view_type=form&cids=1%2C2",
        ),
    ] {
        assert_eq!(policy.shell(input).unwrap(), expected);
    }
    for input in [
        "https://evil.test/odoo",
        "https://user@erp.example.test/odoo",
        "//erp.example.test/odoo",
        "/%2f%2fevil.test",
        "/odoo%0d%0aX:y",
        "/odoo%5cfoo",
        "/odoo?x=%",
        "/odoo?x=%zz",
    ] {
        assert!(policy.normalize(input).is_err(), "{input}");
    }
    for input in [
        "/odoo?action=1&action=2",
        "/odoo/action-42?action=42",
        "/odoo/action-42?view_type=unknown",
        "/odoo/action-42?id=3",
        "/odoo?model=res.users",
        "/odoo#x",
        "/odoo/action-42?cids=1,1",
        "/odoo?action=%2534%2532",
    ] {
        assert!(policy.shell(input).is_err(), "{input}");
    }
}

#[test]
fn erp_shell_routes_and_legacy_bookmarks_are_canonical() {
    let p = RedirectPolicy::new("https://erp.example.test", &["list", "form"]).unwrap();
    for input in ["/erp", "/odoo", "/web"] {
        assert_eq!(p.shell(input).unwrap(), "/erp");
    }
    for input in [
        "/erp/action-8?menu=9&id=8&view_type=form",
        "/odoo/action-8?menu=9&id=8&view_type=form",
        "/web?action=8&menu=9&id=8&view_type=form",
    ] {
        assert_eq!(
            p.shell(input).unwrap(),
            "/erp/action-8?menu=9&id=8&view_type=form"
        );
    }
    for bad in [
        "/erp/action-8?action=8",
        "/erp/action-8?menu=0",
        "//evil.test/erp",
        "/erp/action-8?view_type=unknown",
    ] {
        assert!(p.shell(bad).is_err());
    }
}
