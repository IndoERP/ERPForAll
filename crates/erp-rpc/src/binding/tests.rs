#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::Value;
use erp_orm::rpc::{ParamSpec, Receiver};

use super::*;
use crate::json::parse;

const SEARCH: &[ParamSpec] = &[
    ParamSpec::new("domain", true, "a domain (array)"),
    ParamSpec::new("limit", false, "a non-negative integer"),
];

fn route(path: &str) -> Route {
    Route::parse_json2(path).unwrap()
}

fn bind(path: &str, body: &str, receiver: Receiver) -> Result<Bound, RpcFailure> {
    bind_body(
        &route(path),
        parse(body.as_bytes(), 64).unwrap(),
        receiver,
        SEARCH,
        &CarriedContext::none(),
    )
}

fn refusal(path: &str, body: &str, receiver: Receiver) -> (u16, String) {
    let err = bind(path, body, receiver).unwrap_err();
    (err.status(), err.message())
}

#[test]
fn ids_keep_order_and_duplicates() {
    let b = bind(
        "/json/2/m.x/read",
        r#"{"ids":[3,1,3],"domain":[]}"#,
        Receiver::Records,
    )
    .unwrap();
    assert_eq!(b.ids, [3, 1, 3]);
    let b = bind(
        "/json/2/m.x/read",
        r#"{"ids":7,"domain":[]}"#,
        Receiver::Records,
    )
    .unwrap();
    assert_eq!(b.ids, [7], "a scalar is one record");
    let b = bind("/json/2/m.x/read", r#"{"domain":[]}"#, Receiver::Records).unwrap();
    assert!(b.ids.is_empty());
    let many: Vec<String> = (1..=1000).map(|i| i.to_string()).collect();
    let body = format!(r#"{{"ids":[{}],"domain":[]}}"#, many.join(","));
    assert_eq!(
        bind("/json/2/m.x/read", &body, Receiver::Records)
            .unwrap()
            .ids
            .len(),
        1000
    );
    let max = format!(r#"{{"ids":[{}],"domain":[]}}"#, i64::MAX);
    assert_eq!(
        bind("/json/2/m.x/read", &max, Receiver::Records)
            .unwrap()
            .ids,
        [i64::MAX]
    );
    let cases = [
        (r#"{"ids":0,"domain":[]}"#, "the integer 0"),
        (r#"{"ids":-4,"domain":[]}"#, "the integer -4"),
        (r#"{"ids":[1,0],"domain":[]}"#, "the integer 0 in the list"),
        (r#"{"ids":[1,"2"],"domain":[]}"#, "string in the list"),
        (
            r#"{"ids":[1,9223372036854775808],"domain":[]}"#,
            "integer in the list",
        ),
        (r#"{"ids":"1","domain":[]}"#, "string"),
        (r#"{"ids":{"a":1},"domain":[]}"#, "object"),
        (r#"{"ids":1.5,"domain":[]}"#, "number"),
    ];
    for (body, got) in cases {
        assert_eq!(
            refusal("/json/2/m.x/read", body, Receiver::Records),
            (
                422,
                format!("ids must be a positive integer or a list of positive integers, got {got}")
            ),
            "{body}"
        );
    }
}

#[test]
fn model_methods_refuse_any_truthy_ids_first() {
    for body in [r#"{"ids":[0]}"#, r#"{"ids":5}"#, r#"{"ids":[1],"nope":1}"#] {
        assert_eq!(
            refusal("/json/2/res.users/create", body, Receiver::Model),
            (422, "cannot call res.users.create with ids".to_owned()),
            "{body}"
        );
    }
    let b = bind(
        "/json/2/m.x/search",
        r#"{"ids":[],"domain":[]}"#,
        Receiver::Model,
    )
    .unwrap();
    assert!(b.ids.is_empty());
}

#[test]
fn missing_before_unexpected_with_pythons_texts() {
    assert_eq!(
        refusal("/json/2/res.users/search", "{}", Receiver::Model),
        (422, "missing a required argument: 'domain'".to_owned())
    );
    assert_eq!(
        refusal(
            "/json/2/res.users/search",
            r#"{"bogus":1}"#,
            Receiver::Model
        ),
        (422, "missing a required argument: 'domain'".to_owned())
    );
    assert_eq!(
        refusal(
            "/json/2/res.users/search",
            r#"{"domain":[],"bogus":1}"#,
            Receiver::Model
        ),
        (422, "got an unexpected keyword argument 'bogus'".to_owned())
    );
    assert_eq!(
        refusal(
            "/json/2/res.users/search",
            r#"{"domain":[],"limit":9223372036854775808}"#,
            Receiver::Model
        ),
        (
            422,
            "argument 'limit' of res.users.search: integer 9223372036854775808 is outside the \
             64-bit range"
                .to_owned()
        )
    );
}

#[test]
fn the_path_wins_over_the_body() {
    let b = bind(
        "/json/2/test_http.stargate/search",
        r#"{"__model__":"test_http.galaxy","__method__":"unlink","domain":[]}"#,
        Receiver::Model,
    )
    .unwrap();
    let crate::json::Json::Object(kwargs) = &b.kwargs_json else {
        panic!("kwargs");
    };
    assert_eq!(
        kwargs.iter().map(|(k, _)| &**k).collect::<Vec<_>>(),
        ["domain"]
    );
}

#[test]
fn the_context_is_replaced_and_validated() {
    let ok = bind(
        "/json/2/m.x/search",
        r#"{"domain":[],"context":{"lang":"fr_FR","tz":"Europe/Brussels","active_test":0,
            "allowed_company_ids":[2,1],"default_name":"x","search_default_y":1,"uid":7}}"#,
        Receiver::Model,
    )
    .unwrap();
    let c = &ok.context;
    assert_eq!(c.lang.as_deref(), Some("fr_FR"));
    assert_eq!(c.allowed_company_ids.as_deref(), Some(&[2, 1][..]));
    assert_eq!(c.context.tz(), Some("Europe/Brussels"));
    assert!(!c.context.active_test());
    assert_eq!(c.context.get("default_name"), Some(Value::str("x")));
    assert_eq!(
        c.context.get("uid"),
        Some(Value::Int(7)),
        "opaque, never identity"
    );
    // absent: empty, and `null` is absent
    for body in [r#"{"domain":[]}"#, r#"{"domain":[],"context":null}"#] {
        let b = bind("/json/2/m.x/search", body, Receiver::Model).unwrap();
        assert!(b.context.context.to_map().is_empty(), "{body}");
    }
    let cases: [(&str, u16, &str); 10] = [
        (
            r#"{"domain":[],"context":[1]}"#,
            422,
            "context must be an object, got array",
        ),
        (
            r#"{"domain":[],"context":{"install_mode":true}}"#,
            422,
            "context key 'install_mode' cannot be set by the caller",
        ),
        (
            r#"{"domain":[],"context":{"module":"base"}}"#,
            422,
            "context key 'module' cannot be set by the caller",
        ),
        (
            r#"{"domain":[],"context":{"__copy_data_seen":"test:1"}}"#,
            422,
            "context key '__copy_data_seen' cannot be set by the caller",
        ),
        (
            r#"{"domain":[],"context":{"tz":"Mars/Olympus"}}"#,
            422,
            "Invalid timezone: Mars/Olympus",
        ),
        (
            r#"{"domain":[],"context":{"tz":5}}"#,
            422,
            "context key 'tz': expected a timezone name, got integer",
        ),
        (
            r#"{"domain":[],"context":{"lang":["fr_FR"]}}"#,
            422,
            "context key 'lang': expected a language code, got array",
        ),
        (
            r#"{"domain":[],"context":{"active_test":2}}"#,
            422,
            "context key 'active_test': expected a boolean, got integer",
        ),
        (
            r#"{"domain":[],"context":{"allowed_company_ids":[1,0]}}"#,
            422,
            "context key 'allowed_company_ids': expected a list of positive integers, got array",
        ),
        (
            r#"{"domain":[],"context":{"x":9223372036854775808}}"#,
            422,
            "context key 'x': integer 9223372036854775808 is outside the 64-bit range",
        ),
    ];
    for (body, status, message) in cases {
        assert_eq!(
            refusal("/json/2/m.x/search", body, Receiver::Model),
            (status, message.to_owned()),
            "{body}"
        );
    }
    let tz = bind(
        "/json/2/m.x/search",
        r#"{"domain":[],"context":{"tz":"Mars/Olympus"}}"#,
        Receiver::Model,
    )
    .unwrap_err();
    assert_eq!(tz.name(), "erp.exceptions.ValidationError");
}

#[test]
fn context_bounds() {
    let keys: Vec<String> = (0..MAX_CONTEXT_KEYS)
        .map(|i| format!("\"k{i}\":1"))
        .collect();
    let at = format!(r#"{{"domain":[],"context":{{{}}}}}"#, keys.join(","));
    assert!(bind("/json/2/m.x/search", &at, Receiver::Model).is_ok());
    let keys: Vec<String> = (0..=MAX_CONTEXT_KEYS)
        .map(|i| format!("\"k{i}\":1"))
        .collect();
    let over = format!(r#"{{"domain":[],"context":{{{}}}}}"#, keys.join(","));
    assert_eq!(
        refusal("/json/2/m.x/search", &over, Receiver::Model),
        (
            422,
            "the context has 257 keys; at most 256 are accepted".to_owned()
        )
    );
    let long = "é".repeat(129);
    let body = format!(r#"{{"domain":[],"context":{{"{long}":1}}}}"#);
    assert_eq!(
        refusal("/json/2/m.x/search", &body, Receiver::Model),
        (
            422,
            format!("context key '{}…' is longer than 256 bytes", "é".repeat(32))
        )
    );
}

#[test]
fn carried_keys_fill_only_what_is_absent() {
    let carried = CarriedContext::call_kw([
        ("website_id".to_owned(), Value::Int(1)),
        ("host_id".to_owned(), Value::Int(9)),
    ])
    .unwrap();
    let b = bind_body(
        &route("/json/2/m.x/search"),
        parse(br#"{"domain":[],"context":{"website_id":2}}"#, 64).unwrap(),
        Receiver::Model,
        SEARCH,
        &carried,
    )
    .unwrap();
    assert_eq!(b.context.context.get("website_id"), Some(Value::Int(2)));
    assert_eq!(b.context.context.get("host_id"), Some(Value::Int(9)));
    // the digested context holds what the call runs with: the body's keys, then carried ones
    let crate::json::Json::Object(members) = &b.context_json else {
        panic!("context");
    };
    assert_eq!(
        members
            .iter()
            .map(|(k, v)| (&**k, v.clone()))
            .collect::<Vec<_>>(),
        [
            ("website_id", crate::json::Json::Int(2)),
            ("host_id", crate::json::Json::Int(9))
        ]
    );
    assert_eq!(
        CarriedContext::call_kw([("host_id".to_owned(), Value::list([Value::Int(1)]))])
            .unwrap_err()
            .message(),
        "call_kw's carried 'host_id' must be an integer, a string, a boolean or False"
    );
    assert_eq!(
        CarriedContext::call_kw([("uid".to_owned(), Value::Int(1))])
            .unwrap_err()
            .message(),
        "call_kw carries only website_id and host_id, not 'uid'"
    );
}

/// The data loader's keys are server-only, the load-state keys included (review A3-S1):
/// a caller that sent `install_module` with a forged load state could make `ir.ui.view` skip
/// sibling views in its write check.
#[test]
fn the_loaders_load_state_keys_are_server_only() {
    // every key of the loader's `install_` namespace, listed or not (review A4-R1): the
    // producers' constants are checked against the prefix in `erp-test-models`
    // (`rpc_dispatch.rs::the_loaders_context_keys_are_in_the_server_only_namespace`)
    for key in [
        "install_module",
        "install_pending_modules",
        "install_loaded_names",
        "install_track_batch",
        "install_batch_names",
        "install_batch_positions",
        "install_batch_written",
        "install_demo",
        "install_anything_new",
    ] {
        let body = format!(r#"{{"domain":[],"context":{{"{key}":[]}}}}"#);
        assert_eq!(
            refusal("/json/2/m.x/search", &body, Receiver::Model),
            (
                422,
                format!("context key '{key}' cannot be set by the caller")
            ),
            "{key}"
        );
    }
}

#[test]
fn html_validation_is_a_declared_transport_control() {
    for mode in ["clean", "strict"] {
        let body = format!(r#"{{"domain":[],"html_validation":"{mode}"}}"#);
        bind("/json/2/m.x/search", &body, Receiver::Model).unwrap();
    }
    for invalid in ["false", "null", "42", "{}", "[]", "\"off\""] {
        let body = format!(r#"{{"domain":[],"html_validation":{invalid}}}"#);
        assert_eq!(
            refusal("/json/2/m.x/search", &body, Receiver::Model),
            (
                422,
                "html_validation must be 'clean' or 'strict'".to_owned()
            )
        );
    }
}

#[test]
fn html_validation_is_not_a_model_argument_or_context_bypass() {
    let path = "/json/2/m.x/search";
    let mut digests = Vec::new();
    for control in [
        "",
        ",\"html_validation\":\"strict\"",
        ",\"html_validation\":\"clean\"",
    ] {
        let body = format!(r#"{{"domain":[]{control}}}"#);
        let bound = bind(path, &body, Receiver::Model).unwrap();
        assert_eq!(bound.args.remaining().collect::<Vec<_>>(), ["domain"]);
        digests.push(crate::idempotency::request_digest(
            "m.x",
            "search",
            &bound.ids_json,
            &bound.context_json,
            &bound.kwargs_json,
        ));
    }
    assert_ne!(digests[0], digests[1]);
    assert_ne!(digests[1], digests[2]);
    assert_ne!(digests[0], digests[2]);
    assert_eq!(
        refusal(
            path,
            r#"{"domain":[],"context":{"html_validation":"clean"}}"#,
            Receiver::Model
        ),
        (
            422,
            "context key 'html_validation' cannot be set by the caller".to_owned()
        )
    );
}
