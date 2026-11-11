#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::boundary::{self, BoundaryError};

#[test]
fn local_navigation_has_a_closed_grammar() {
    let policy =
        erp_server::redirect::RedirectPolicy::new("https://erp.example.test", &["list", "form"])
            .unwrap();
    for value in [
        "/odoo",
        "/odoo/action-42",
        "/odoo/action-42?id=4&view_type=form&cids=1,2",
    ] {
        assert_eq!(
            policy.shell(value).unwrap(),
            value.replace("/odoo", "/erp").replace(",", "%2C")
        );
    }
    for value in [
        "https://evil.invalid",
        "//evil.invalid",
        "/\\evil.invalid",
        "/odoo#token=x",
        "/odoo?token=x",
        "/odoo/action-0",
        "/odoo/action-01",
        "/odoo/action-9223372036854775808",
        "/odoo?view_type=unknown",
        "/odoo/action-42?id=4",
        "/odoo?model=res.users",
        "/odoo?id=1&id=2",
        "/odoo%0d%0aX:y",
    ] {
        let error = policy.shell(value).unwrap_err();
        assert_eq!(error, BoundaryError::Redirect);
        assert_eq!(
            error.to_string(),
            "redirect must be an allowed local shell URL"
        );
    }
}

#[test]
fn authentication_cookie_duplicates_never_choose_a_winner() {
    assert_eq!(
        boundary::cookie(&["a=1; session_id=db.secret"], "session_id").unwrap(),
        Some("db.secret")
    );
    assert_eq!(boundary::cookie(&["a=1"], "session_id").unwrap(), None);
    for headers in [
        vec!["session_id=a; session_id=b"],
        vec!["session_id=a", "session_id=a"],
        vec!["session_id="],
        vec!["session_id=\"quoted\""],
    ] {
        let error = boundary::cookie(&headers, "session_id").unwrap_err();
        assert_eq!(error, BoundaryError::Cookie);
        assert_eq!(
            error.to_string(),
            "ambiguous or malformed authentication cookie"
        );
    }
}

#[test]
fn database_cookie_and_header_must_agree_with_the_allowlist() {
    assert_eq!(
        boundary::database("erp", Some("erp"), Some("erp")).unwrap(),
        "erp"
    );
    assert_eq!(boundary::database("erp", None, None).unwrap(), "erp");
    assert_eq!(
        boundary::database("erp", Some("other"), Some("erp")).unwrap_err(),
        BoundaryError::DatabaseMismatch
    );
    assert_eq!(
        boundary::database("erp", Some("other"), None).unwrap_err(),
        BoundaryError::Database
    );
}

#[test]
fn unsafe_cookie_json_refuses_cross_site_and_missing_metadata() {
    assert_eq!(
        boundary::same_origin_fetch(
            Some("same-origin"),
            Some("https://erp.invalid"),
            "https://erp.invalid"
        ),
        Ok(())
    );
    assert_eq!(
        boundary::same_origin_fetch(Some("same-origin"), None, "https://erp.invalid"),
        Ok(())
    );
    for (site, origin) in [
        (None, None),
        (Some("same-site"), None),
        (Some("cross-site"), None),
        (Some("same-origin"), Some("https://other.invalid")),
    ] {
        assert_eq!(
            boundary::same_origin_fetch(site, origin, "https://erp.invalid"),
            Err(BoundaryError::SameOrigin)
        );
    }
}

#[test]
fn bearer_headers_are_single_bounded_and_secret_redacted() {
    assert_eq!(boundary::bearer(&[]).unwrap(), None);
    assert_eq!(
        boundary::bearer(&["bEaReR erpk_sample"]).unwrap(),
        Some("erpk_sample")
    );
    let large = format!("Bearer {}", "x".repeat(1025));
    for headers in [
        vec!["Bearer a", "Bearer b"],
        vec!["Basic a"],
        vec!["Bearer"],
        vec!["Bearer "],
        vec!["Bearer a b"],
        vec!["Bearer secret\n"],
        vec!["Bearer clé"],
        vec![large.as_str()],
    ] {
        let error = boundary::bearer(&headers).unwrap_err();
        assert_eq!(error.to_string(), "Invalid apikey");
        assert_eq!(error, BoundaryError::Authorization);
        assert!(!format!("{error:?}").contains("secret"));
    }
}
