//! JSON-2 dispatch in-process: routing, exposure, gates, binding, context,
//! strict JSON and the error policy, under the restricting fixture policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use erp_orm::read_group::ReadGroup;
use erp_orm::rpc::{Effect, Exposure, IntoRpc};
use erp_orm::web::{FormattedReadGroup, FormattedReadGroupingSets, ProgressBar, WebReadGroup};
use erp_orm::{KeyKind, ModelAllow, UserId};
use erp_rpc::{Caller, IdempotencyKey, RpcCall, RpcConfig};
use erp_sdk::{Domain, Value};
use serde_json::json;
use std::sync::Arc;
use support::rpc::*;

fn val_map<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Map(Arc::new(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    ))
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::False => serde_json::Value::Bool(false),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::json!(*i),
        Value::Float(f) => serde_json::json!(*f),
        Value::Decimal(d) => serde_json::json!(d.to_string()),
        Value::Str(s) => serde_json::Value::String(s.to_string()),
        Value::Bytes(b) => serde_json::Value::String(erp_fields::base64::encode(b)),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::DateTime(d) => serde_json::Value::String(d.to_string()),
        Value::Json(j) => (**j).clone(),
        Value::Range(_) => panic!("unexpected range in rpc result"),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map.iter() {
                obj.insert(k.clone(), value_to_json(val));
            }
            serde_json::Value::Object(obj)
        }
        Value::Row(row) => {
            let mut obj = serde_json::Map::new();
            for entry in row.iter() {
                let (k, val) = entry.expect("row entry");
                obj.insert(k.to_owned(), value_to_json(&val));
            }
            serde_json::Value::Object(obj)
        }
        Value::Rows(rows) => serde_json::Value::Array(
            rows.iter()
                .expect("rows")
                .map(|row| value_to_json(&Value::Row(row)))
                .collect(),
        ),
        other => panic!("unexpected value in rpc result: {other:?}"),
    }
}

#[tokio::test]
async fn json2_success_path() {
    let h = harness().await;
    let found = call(
        &h,
        "/json/2/res.users/search",
        &format!(r#"{{"domain": [["id", "=", {ALICE}]]}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&found), json!([ALICE]));
    for ids in [format!("[{ALICE}]"), ALICE.to_string()] {
        let read = call(
            &h,
            "/json/2/res.users/read",
            &format!(r#"{{"ids": {ids}, "fields": ["display_name"]}}"#),
            alice(),
        )
        .await;
        assert_eq!(
            ok(&read),
            json!([{"id": ALICE, "display_name": "alice"}]),
            "ids {ids}"
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_request_errors() {
    let h = harness().await;
    let r = call(&h, "/json/2/res.users/search", "not json", alice()).await;
    assert_error(
        &r,
        400,
        "werkzeug.exceptions.BadRequest",
        "could not parse the body as json: Expecting value: line 1 column 1 (char 0)",
    );
    assert_eq!(json(&r)["arguments"], json!([json(&r)["message"], 400]));
    for body in ["null", "{}"] {
        let r = call(&h, "/json/2/res.users/search", body, alice()).await;
        assert_error(
            &r,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "missing a required argument: 'domain'",
        );
    }
    let r = call(&h, "/json/2/res.users/create", r#"{"ids": [0]}"#, alice()).await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.create with ids",
    );
    let r = call(&h, "/json/2/res.users", "{}", alice()).await;
    assert_error(
        &r,
        404,
        "werkzeug.exceptions.NotFound",
        "Did you mean POST /erp/json/2/<model>/<method>?",
    );
    let r = call(&h, "/json/2/res.nope/search", r#"{"domain": []}"#, alice()).await;
    assert_error(
        &r,
        404,
        "werkzeug.exceptions.NotFound",
        "the model 'res.nope' does not exist",
    );
    let r = call(&h, "/json/2/res.users/_compute_display_name", "{}", alice()).await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "Private methods (such as 'res.users._compute_display_name') cannot be called remotely.",
    );
    let r = call(
        &h,
        "/json/2/res.users/search",
        r#"{"domain": [["bad field", "=", 1]]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field res.users.bad field in condition ('bad field', '=', 1)",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.model_a/search",
        r#"{"domain": []}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn path_parameters_win_over_the_body() {
    let h = harness().await;
    for name in ["SG-1", "SG-2"] {
        sql(
            &h,
            &format!("INSERT INTO test_http_stargate (name) VALUES ('{name}') RETURNING id::text"),
        )
        .await;
    }
    sql(
        &h,
        "INSERT INTO test_http_galaxy (name) VALUES ('Milky Way') RETURNING id::text",
    )
    .await;
    let r = call(
        &h,
        "/json/2/test_http.stargate/search",
        r#"{"__model__": "test_http.galaxy", "__method__": "unlink", "domain": []}"#,
        alice(),
    )
    .await;
    let stargates: Vec<i64> = sql(&h, "SELECT id::text FROM test_http_stargate ORDER BY id")
        .await
        .into_iter()
        .map(|v| v.unwrap().parse().unwrap())
        .collect();
    assert_eq!(ok(&r), json!(stargates));
    assert_eq!(count(&h, "test_http_galaxy").await, 1, "galaxy untouched");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn binary_values_are_strict_base64() {
    let h = harness().await;
    let r = call(
        &h,
        "/json/2/test_rpc.effects/binary_echo",
        r#"{"raw": "eCAgAS0="}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!({"content": "eCAgAS0=", "size": 5}));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/binary_echo",
        r#"{"raw": "WCABIQ=="}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!({"content": "WCABIQ==", "size": 4}));
    for (raw, problem) in [
        ("WCAB IQ=", "invalid character ' ' at position 4"),
        ("WCABIQ", "length 6 is not a multiple of 4"),
        ("WCABIQ-_", "invalid character '-' at position 6"),
    ] {
        let r = call(
            &h,
            "/json/2/test_rpc.effects/binary_echo",
            &format!(r#"{{"raw": "{raw}"}}"#),
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "erp.exceptions.ValidationError",
            &format!(
                "argument 'raw' of test_rpc.effects.binary_echo: Invalid base64 value: {problem}"
            ),
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_context_is_replaced() {
    let h = harness().await;
    let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await;
    let body = ok(&r);
    assert_eq!(body["lang"], json!(false));
    assert_eq!(body["keys"], json!([]));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        r#"{"context": {"lang": "fr_FR"}}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r)["lang"], "fr_FR");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn check_access_is_private_and_has_access_callable() {
    let h = harness().await;
    let id = effect(&h, "e", 1).await;
    let r = call(
        &h,
        "/json/2/test_rpc.effects/check_access",
        r#"{"operation": "read"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "Private methods (such as 'test_rpc.effects.check_access') cannot be called remotely.",
    );
    let body = format!(r#"{{"ids": [{id}], "operation": "write"}}"#);
    let r = call(&h, "/json/2/test_rpc.effects/has_access", &body, alice()).await;
    assert_eq!(ok(&r), json!(true));
    let r = call(&h, "/json/2/test_rpc.effects/has_access", &body, user(BOB)).await;
    assert_eq!(ok(&r), json!(false));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn override_rules() {
    let h = harness_with(
        &[&erp_test_models::test_rpc::ext::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    let r = call(&h, "/json/2/test_rpc.effects/shown", "{}", alice()).await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "Private methods (such as 'test_rpc.effects.shown') cannot be called remotely.",
    );
    let model = h.orm().registry().model_id("test_rpc.effects").unwrap();
    match h.orm().exposure().get(model, "widened") {
        Some(Exposure::Exposed(e)) => {
            assert_eq!(e.effect(), Effect::External);
            assert_eq!(e.effect_from().addon(), "test_rpc_ext");
        }
        other => panic!("widened: {other:?}"),
    }
    match h.orm().exposure().get(model, "shown") {
        Some(Exposure::Hidden { by }) => assert_eq!(by.addon(), "test_rpc_ext"),
        other => panic!("shown: {other:?}"),
    }
    // an override without #[rpc] keeps the parent's read mode
    let r = call(&h, "/json/2/test_rpc.effects/inherited_read", "{}", alice()).await;
    assert_eq!(ok(&r), json!(true));
    h.cleanup().await.unwrap();

    let registry = std::sync::Arc::new(
        erp_sdk::RegistryBuilder::new([
            &erp_test_models::base_users::ADDON,
            &erp_test_models::test_orm::ADDON,
            &erp_test_models::test_rpc::ADDON,
            &erp_test_models::test_rpc::narrow::ADDON,
        ])
        .with_decls(crate::currency_fixture::decls())
        .build()
        .unwrap(),
    );
    let pool = erp_orm::pool::create_pool(erp_testkit::tokio_postgres::Config::new(), 1).unwrap();
    let err = erp_orm::Orm::builder(registry, pool)
        .policy(erp_testkit::RulesPolicy::new(FixturePolicy).unwrap())
        .build()
        .err()
        .expect("narrowing must fail the build");
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    let message = err.message();
    assert!(
        message.starts_with(
            "test_rpc.effects.bump: addon 'test_rpc_narrow' declares effect = read below effect \
             = write declared by 'test_rpc' at "
        ) && message.ends_with("; an override may only widen an effect"),
        "{message}"
    );
}

#[tokio::test]
async fn read_only_keys_call_only_read_methods() {
    let h = harness().await;
    let reader = || alice_key(scope(7, "reader", ModelAllow::All, true));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "x"}"#,
        reader(),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'test_rpc.effects/bump' (effect \
         'write').",
    );
    assert_eq!(r.attempts(), 0, "refused before any transaction");
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    for (method, effect) in [("burn", "destructive"), ("notify", "external")] {
        let r = call(
            &h,
            &format!("/json/2/test_rpc.effects/{method}"),
            "{}",
            reader(),
        )
        .await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "The API key 'reader' is read-only: it may not call \
                 'test_rpc.effects/{method}' (effect '{effect}')."
            ),
        );
    }
    let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", reader()).await;
    assert_eq!(ok(&r)["read_only"], json!(true));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_scope_allowlist_and_the_subjects_rules() {
    let h = harness().await;
    let bot = || {
        alice_key(scope(
            8,
            "effects bot",
            ModelAllow::named(["test_rpc.effects"]).unwrap(),
            false,
        ))
    };
    let r = call(
        &h,
        "/json/2/test_http.stargate/search",
        r#"{"domain": []}"#,
        bot(),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "The API key 'effects bot' may not call 'search' on 'Stargate' (test_http.stargate): \
         outside its scope.",
    );
    let mine = effect(&h, "mine", 1).await;
    effect(&h, "other company", 2).await;
    let r = call(
        &h,
        "/json/2/test_rpc.effects/search_read",
        r#"{"domain": [], "fields": ["name"]}"#,
        bot(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!([{"id": mine, "name": "mine"}]),
        "the record rule of the subject applies"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn agent_and_service_keys_wait_for_principal() {
    let h = harness().await;
    for (kind, name) in [(KeyKind::Agent, "agent"), (KeyKind::Service, "service")] {
        let caller = Caller::ApiKey {
            kind,
            key: erp_orm::ApiKeyId::new(3).unwrap(),
        };
        let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", caller).await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessDenied",
            &format!("API keys of kind '{name}' are not accepted until a later release"),
        );
        assert_eq!(r.attempts(), 0);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_debug_field_needs_the_switch_and_the_capability() {
    let sentinel = "sentinel-7f3a";
    let body = format!(r#"{{"secret": "{sentinel}"}}"#);
    for (debug_on, caller, shown) in [
        (false, user(ADMIN), false),
        (true, alice(), false),
        (true, user(ADMIN), true),
    ] {
        let h = harness_with(
            &[],
            RpcConfig::builder().error_debug(debug_on).build().unwrap(),
            false,
        )
        .await;
        let r = h
            .service()
            .call(
                RpcCall::json2(
                    "/json/2/test_rpc.effects/secret_failure",
                    body.as_bytes().to_vec(),
                    caller,
                )
                .correlation(erp_rpc::CorrelationId::from_u128(42)),
            )
            .await;
        assert_error(
            &r,
            500,
            "erp.database",
            "A database error occurred (SQLSTATE 22P02).",
        );
        let text = String::from_utf8_lossy(r.body()).into_owned();
        assert!(!text.contains(sentinel), "never the bound value: {text}");
        let debug = json(&r)["debug"].as_str().unwrap().to_owned();
        if shown {
            assert_eq!(
                debug,
                "kind: erp.database (500)\nerror kind: Database\nsqlstate: 22P02\nerror: A \
                 database error occurred (SQLSTATE 22P02).\ncaused by: SQLSTATE 22P02\n\
                 statement: SELECT $1::text::int\nspan: rpc.call \
                 test_rpc.effects.secret_failure\ncorrelation: \
                 00000000-0000-0000-0000-00000000002a"
            );
        } else {
            assert_eq!(debug, "", "debug_on {debug_on}");
        }
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn context_rules() {
    let h = harness().await;
    let probe = |context: &str| format!(r#"{{"context": {context}}}"#);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"install_mode": true}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "context key 'install_mode' cannot be set by the caller",
    );
    // the keys that switch tracking off are the server's: a client cannot silence the change log
    for key in erp_sdk::TRACKING_DISABLE_KEYS {
        let r = call(
            &h,
            "/json/2/test_rpc.effects/probe",
            &probe(&format!(r#"{{"{key}": true}}"#)),
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            &format!("context key '{key}' cannot be set by the caller"),
        );
    }
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"x_custom": [1, {"a": null}], "tz": "Europe/Brussels"}"#),
        alice(),
    )
    .await;
    let body = ok(&r);
    assert_eq!(body["keys"], json!(["tz", "x_custom"]));
    assert_eq!(body["tz"], "Europe/Brussels");
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"lang": "xx_XX"}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.UserError",
        "Invalid language code: xx_XX",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"tz": "Mars/Olympus"}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "Invalid timezone: Mars/Olympus",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"allowed_company_ids": [3]}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "One of the selected companies does not exist or is not one of yours.",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"allowed_company_ids": [2, 1]}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&r)["companies"], json!([2, 1]));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(r#"{"allowed_company_ids": [1, 1]}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "context key 'allowed_company_ids': company 1 is listed twice",
    );
    let many: Vec<String> = (0..257).map(|i| format!(r#""k{i}": 1"#)).collect();
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        &probe(&format!("{{{}}}", many.join(", "))),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "the context has 257 keys; at most 256 are accepted",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn strict_json() {
    let h = harness().await;
    let cases: [(&[u8], &str); 5] = [
        (
            br#"{"domain": [], "domain": []}"#,
            "could not parse the body as json: duplicate key 'domain': line 1 column 16 (char 15)",
        ),
        (b"[1]", "the JSON-2 body must be an object, got array"),
        (b"\"text\"", "the JSON-2 body must be an object, got string"),
        (
            "\u{feff}{}".as_bytes(),
            "could not parse the body as json: Unexpected UTF-8 BOM (decode using utf-8-sig): \
             line 1 column 1 (char 0)",
        ),
        (
            b"{\r\n\"domain\": \r\n}",
            "could not parse the body as json: Expecting value: line 3 column 1 (char 15)",
        ),
    ];
    for (body, message) in cases {
        let r = h
            .service()
            .call(RpcCall::json2(
                "/json/2/res.users/search",
                body.to_vec(),
                alice(),
            ))
            .await;
        assert_error(&r, 400, "werkzeug.exceptions.BadRequest", message);
    }
    let deep_ok = format!(r#"{{"domain": {}{}}}"#, "[".repeat(63), "]".repeat(63));
    let r = call(&h, "/json/2/res.users/search", &deep_ok, alice()).await;
    assert_ne!(r.status(), 400, "depth 64 parses");
    let too_deep = format!(r#"{{"domain": {}{}}}"#, "[".repeat(64), "]".repeat(64));
    let r = call(&h, "/json/2/res.users/search", &too_deep, alice()).await;
    assert_error(
        &r,
        400,
        "werkzeug.exceptions.BadRequest",
        "could not parse the body as json: nesting deeper than 64 levels: line 1 column 75 \
         (char 74)",
    );
    // an empty body is no parameters
    let r = call(&h, "/json/2/test_rpc.effects/probe", "", alice()).await;
    assert_eq!(ok(&r)["read_only"], json!(true));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_core_methods_answer_501() {
    let h = harness().await;
    // (`has_group` left the table: `base` exposes it)
    let r = call(&h, "/json/2/res.users/has_groups", "{}", alice()).await;
    assert_error(
        &r,
        501,
        "erp.not_implemented",
        "res.users.has_groups is not supported until a later release.",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/internal_helper",
        "{}",
        alice(),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "Private methods (such as 'test_rpc.effects.internal_helper') cannot be called remotely.",
    );
    let r = call(&h, "/json/2/test_rpc.effects/mro", "{}", alice()).await;
    assert_eq!(
        r.status(),
        403,
        "the reference ERP's unsafe attribute names"
    );
    let r = call(&h, "/json/2/test_rpc.effects/nope", "{}", alice()).await;
    assert_error(
        &r,
        404,
        "werkzeug.exceptions.NotFound",
        "The method 'test_rpc.effects.nope' does not exist",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_cursor_mode_follows_the_effect() {
    let h = harness_with(
        &[&erp_test_models::test_rpc::ext::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await;
    let body = ok(&r);
    assert_eq!(
        (body["read_only"].clone(), body["intent_read"].clone()),
        (json!(true), json!(true))
    );
    let r = call(&h, "/json/2/test_rpc.effects/probe_write", "{}", alice()).await;
    let body = ok(&r);
    assert_eq!(
        (body["read_only"].clone(), body["intent_read"].clone()),
        (json!(false), json!(false))
    );
    let r = call(&h, "/json/2/test_rpc.effects/inherited_read", "{}", alice()).await;
    assert_eq!(ok(&r), json!(true));
    let r = call(&h, "/json/2/test_rpc.nope/probe", "{}", alice()).await;
    assert_error(
        &r,
        404,
        "werkzeug.exceptions.NotFound",
        "the model 'test_rpc.nope' does not exist",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn idempotency_key_refusals() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a");
    let r = call_keyed(&h, "/json/2/test_rpc.effects/probe", "{}", alice(), &k).await;
    assert_error(
        &r,
        422,
        "erp.idempotency_not_allowed",
        "Idempotency-Key is accepted only on calls that change data; test_rpc.effects.probe has \
         effect 'read'.",
    );
    for (caller, kind) in [(Caller::Public, "public"), (Caller::Link, "link")] {
        let r = call_keyed(
            &h,
            "/json/2/test_rpc.effects/bump",
            r#"{"name": "x"}"#,
            caller,
            &k,
        )
        .await;
        assert_error(
            &r,
            422,
            "erp.idempotency_not_allowed",
            &format!(
                "Idempotency-Key is not accepted from {kind} callers: they have no private key \
                 space."
            ),
        );
    }
    let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", Caller::Public).await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessDenied",
        "public callers are not accepted until a later release",
    );
    let bad = IdempotencyKey::parse("nope").unwrap_err();
    assert_error(
        &h.service().reject(bad),
        400,
        "werkzeug.exceptions.BadRequest",
        "Idempotency-Key must be a UUID (RFC 9562)",
    );
    let twice = IdempotencyKey::from_headers(&[k.as_str(), k.as_str()]).unwrap_err();
    assert_error(
        &h.service().reject(twice),
        400,
        "werkzeug.exceptions.BadRequest",
        "Idempotency-Key must be sent at most once",
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    h.cleanup().await.unwrap();
}

/// Arguments: defaults, optional parameters and wrong types (design note §6.3).
#[tokio::test]
async fn argument_binding() {
    let h = harness().await;
    let r = call(
        &h,
        "/json/2/test_rpc.effects/echo",
        r#"{"name": "é"}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!({"name": "é", "limit": 10, "flag": false}));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/echo",
        r#"{"name": "n", "limit": 3, "flag": true}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!({"name": "n", "limit": 3, "flag": true}));
    let r = call(
        &h,
        "/json/2/test_rpc.effects/echo",
        r#"{"name": 5}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'name' of test_rpc.effects.echo: expected a string, got integer",
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/echo",
        r#"{"name": "n", "limit": 1.5}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'limit' of test_rpc.effects.echo: expected an integer, got number",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_058_recordset_api_methods_refused_remotely() {
    let h = harness().await;
    let methods = [
        "browse",
        "exists",
        "mapped",
        "filtered",
        "sorted",
        "ensure_one",
        "lock_for_update",
    ];
    for method in methods {
        let r = call(
            &h,
            &format!("/json/2/test_orm.partner/{method}"),
            "{}",
            alice(),
        )
        .await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "Private methods (such as 'test_orm.partner.{method}') cannot be called remotely."
            ),
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_061_overrides_of_private_methods_stay_private() {
    let h = harness().await;
    let methods = ["filtered", "private_method", "_create", "init"];
    for method in methods {
        let r = call(
            &h,
            &format!("/json/2/test_rpc.model_a/{method}"),
            "{}",
            alice(),
        )
        .await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "Private methods (such as 'test_rpc.model_a.{method}') cannot be called remotely."
            ),
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_read_group() {
    let h = harness().await;

    // 1. Success path: expected result derived from typed API in the same transaction
    let r = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["__count"]}"#,
        alice(),
    )
    .await;
    let expected = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let mut rg = ReadGroup::new(Domain::TRUE);
        rg.groupby = vec!["active".to_string()];
        rg.aggregates = vec!["__count".to_string()];
        rg.external = true;
        let res = users
            .read_group(&mut tx, rg)
            .await
            .unwrap()
            .into_rpc()
            .unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r), expected);

    // Optional parameters (limit, order, having, offset) grouping by field with >=2 values (login)
    let r_paged = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "groupby": ["login"], "aggregates": ["__count"], "having": [["__count", ">=", 1]], "offset": 1, "limit": 2, "order": "login asc"}"#,
        alice(),
    )
    .await;
    let expected_paged = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let mut rg = ReadGroup::new(Domain::TRUE);
        rg.groupby = vec!["login".to_string()];
        rg.aggregates = vec!["__count".to_string()];
        rg.having = vec![Value::list([
            Value::str("__count"),
            Value::str(">="),
            Value::Int(1),
        ])];
        rg.offset = 1;
        rg.limit = Some(2);
        rg.order = Some("login asc".to_string());
        rg.external = true;
        let res = users
            .read_group(&mut tx, rg)
            .await
            .unwrap()
            .into_rpc()
            .unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r_paged), expected_paged);

    // 2. Policy refusal: Bob cannot read test_rpc.model_a
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/read_group",
        r#"{"domain": [], "groupby": ["name"], "aggregates": ["__count"]}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // Read-only API key is refused on effect 'write'
    let reader = alice_key(scope(8, "reader", ModelAllow::All, true));
    let r_key = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["__count"]}"#,
        reader,
    )
    .await;
    assert_error(
        &r_key,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'res.users/read_group' (effect 'write').",
    );

    // Field-level access refusal: group-restricted field -> 403 AccessError
    let policy = erp_testkit::FieldPolicy::new().forbid("test_fa.item", "forbidden3");
    let h_fa = harness_with_policy(
        &[&erp_test_models::test_field_access::ADDON],
        policy,
        RpcConfig::default(),
        false,
    )
    .await;
    let r_field_access = call(
        &h_fa,
        "/json/2/test_fa.item/read_group",
        r#"{"domain": [], "groupby": ["forbidden3"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_field_access,
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"forbidden3\" on Object For Test Access Right (test_fa.item). Please contact your system administrator.\n\nOperation: read",
    );
    h_fa.cleanup().await.unwrap();

    // 3. Validation refusals: invalid field, bad aggregate spec, capped list length
    let r_bad_field = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "groupby": ["nope"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_field,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users'",
    );

    let r_bad_agg = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["nope:bad"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_agg,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users' for 'nope:bad'.",
    );

    let long_groupby: Vec<String> = (0..65).map(|i| format!("f{i}")).collect();
    let r_long = call(
        &h,
        "/json/2/res.users/read_group",
        &serde_json::to_string(&json!({"domain": [], "groupby": long_groupby})).unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_long,
        422,
        "erp.exceptions.ValidationError",
        "'groupby' accepts at most 64 entries, got 65",
    );

    // 4. Missing required parameter (domain)
    let r_missing = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"groupby": ["active"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'domain'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"domain": [], "unknown_param": 1}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'unknown_param'",
    );

    // 6. Called with ids (Receiver::Model rejects ids)
    let r_ids = call(
        &h,
        "/json/2/res.users/read_group",
        r#"{"ids": [1], "domain": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.read_group with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_formatted_read_group() {
    let h = harness().await;

    // 1. Success path: expected result derived from typed API
    let r = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["__count"]}"#,
        alice(),
    )
    .await;
    let expected = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let call = FormattedReadGroup {
            domain: Domain::TRUE,
            groupby: vec!["active".to_string()],
            aggregates: vec!["__count".to_string()],
            having: Vec::new(),
            offset: 0,
            limit: None,
            order: None,
            count_groups: false,
        };
        let res = users
            .formatted_read_group(&mut tx, call)
            .await
            .unwrap()
            .into_rpc()
            .unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r), expected);

    // Optional parameters (limit, offset, having, order)
    let r_paged = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"domain": [], "groupby": ["login"], "aggregates": ["__count"], "having": [["__count", ">=", 1]], "offset": 1, "limit": 1, "order": "login asc"}"#,
        alice(),
    )
    .await;
    let expected_paged = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let call = FormattedReadGroup {
            domain: Domain::TRUE,
            groupby: vec!["login".to_string()],
            aggregates: vec!["__count".to_string()],
            having: vec![Value::list([
                Value::str("__count"),
                Value::str(">="),
                Value::Int(1),
            ])],
            offset: 1,
            limit: Some(1),
            order: Some("login asc".to_string()),
            count_groups: false,
        };
        let res = users
            .formatted_read_group(&mut tx, call)
            .await
            .unwrap()
            .into_rpc()
            .unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r_paged), expected_paged);

    // 2. Policy refusal: Bob cannot read test_rpc.model_a
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/formatted_read_group",
        r#"{"domain": []}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusals: invalid field, bad aggregate spec, capped list length
    let r_bad_field = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"domain": [], "groupby": ["nope"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_field,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users'",
    );

    let r_bad_agg = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["nope:bad"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_agg,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users' for 'nope:bad'.",
    );

    let long_aggs: Vec<String> = (0..65).map(|i| format!("f{i}:sum")).collect();
    let r_long = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        &serde_json::to_string(&json!({"domain": [], "aggregates": long_aggs})).unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_long,
        422,
        "erp.exceptions.ValidationError",
        "'aggregates' accepts at most 64 entries, got 65",
    );

    // 4. Missing required parameter (domain)
    let r_missing = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"groupby": ["active"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'domain'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"domain": [], "bad": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'bad'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/res.users/formatted_read_group",
        r#"{"ids": [1], "domain": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.formatted_read_group with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_formatted_read_grouping_sets() {
    let h = harness().await;

    // 1. Success path: expected result derived from typed API
    let r = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": [["active"], []], "aggregates": ["__count"]}"#,
        alice(),
    )
    .await;
    let expected = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let call = FormattedReadGroupingSets {
            domain: Domain::TRUE,
            grouping_sets: vec![vec!["active".to_string()], vec![]],
            aggregates: vec!["__count".to_string()],
            order: None,
        };
        let res = users
            .formatted_read_grouping_sets(&mut tx, call)
            .await
            .unwrap()
            .into_rpc()
            .unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r), expected);

    // 2. Policy refusal
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": [["name"]]}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusals: empty grouping_sets, invalid field, bad aggregate spec, capped sets
    let r_empty = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_empty,
        422,
        "erp.exceptions.ValidationError",
        "The 'grouping_sets' parameter cannot be empty.",
    );

    let r_bad_field = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": [["nope"]]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_field,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users'",
    );

    let r_bad_agg = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": [["active"]], "aggregates": ["nope:bad"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_agg,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users' for 'nope:bad'.",
    );

    let long_sets: Vec<Vec<String>> = (0..65).map(|_| vec!["active".to_string()]).collect();
    let r_long = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        &serde_json::to_string(&json!({"domain": [], "grouping_sets": long_sets})).unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_long,
        422,
        "erp.exceptions.ValidationError",
        "'grouping_sets' accepts at most 64 sets, got 65",
    );

    // 4. Missing required parameters
    let r_missing_domain = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"grouping_sets": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_domain,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'domain'",
    );

    let r_missing_sets = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_sets,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'grouping_sets'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"domain": [], "grouping_sets": [], "extra": 1}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/res.users/formatted_read_grouping_sets",
        r#"{"ids": [1], "domain": [], "grouping_sets": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.formatted_read_grouping_sets with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_web_read_group() {
    let h = harness().await;

    // 1. Success path: expected result derived from typed API
    let r = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"]}"#,
        alice(),
    )
    .await;
    let expected = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let call = WebReadGroup::new(Domain::TRUE, vec!["active".to_string()]);
        let res = users.web_read_group(&mut tx, call).await.unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r), expected);

    // Non-default parameters: unfold_read_default_limit, opening_info
    let r_custom = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"], "unfold_read_default_limit": 5, "opening_info": [{"value": true, "folded": true}]}"#,
        alice(),
    )
    .await;
    let expected_custom = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let mut call = WebReadGroup::new(Domain::TRUE, vec!["active".to_string()]);
        call.unfold_read_default_limit = Some(5);
        call.opening_info = Some(vec![val_map([
            ("value", Value::Bool(true)),
            ("folded", Value::Bool(true)),
        ])]);
        let res = users.web_read_group(&mut tx, call).await.unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r_custom), expected_custom);

    // 2. Policy refusal
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/web_read_group",
        r#"{"domain": [], "groupby": ["name"]}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusals: empty groupby, invalid field, bad aggregate spec, opening_info non-objects, capped groupby
    let r_empty_groupby = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_empty_groupby,
        422,
        "erp.exceptions.ValidationError",
        "Need at least one item to groupby",
    );

    let r_bad_opening = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"], "opening_info": ["oops", 1]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_opening,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'opening_info' of res.users.web_read_group: expected an array of objects, got string",
    );

    let r_null_offset = call(
        &h,
        "/json/2/res.users/web_search_read",
        r#"{"domain": [], "specification": {}, "offset": null}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_null_offset,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'offset' of res.users.web_search_read: expected a non-negative integer, got null",
    );

    let r_bad_unfold = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"], "unfold_read_default_limit": -1}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_unfold,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'unfold_read_default_limit' of res.users.web_read_group: expected a non-negative integer or null, got -1",
    );

    let r_bad_field = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["nope"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_field,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users'",
    );

    let r_bad_agg = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"], "aggregates": ["nope:bad"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_agg,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users' for 'nope:bad'.",
    );

    let long_gb: Vec<String> = (0..65).map(|i| format!("f{i}")).collect();
    let r_long = call(
        &h,
        "/json/2/res.users/web_read_group",
        &serde_json::to_string(&json!({"domain": [], "groupby": long_gb})).unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_long,
        422,
        "erp.exceptions.ValidationError",
        "'groupby' accepts at most 64 entries, got 65",
    );

    // 4. Missing required parameters
    let r_missing_domain = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"groupby": ["active"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_domain,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'domain'",
    );

    let r_missing_groupby = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_groupby,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'groupby'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"domain": [], "groupby": ["active"], "invalid": "val"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'invalid'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/res.users/web_read_group",
        r#"{"ids": [1], "domain": [], "groupby": ["active"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.web_read_group with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_read_progress_bar() {
    let h = harness().await;

    // 1. Success path: expected result derived from typed API
    let r = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active", "progress_bar": {"field": "login", "colors": {"alice": "success", "bob": "warning"}}}"#,
        alice(),
    )
    .await;
    let expected = {
        let mut tx = h.orm().begin(UserId::new(ALICE).unwrap()).await.unwrap();
        let users = tx.env().records("res.users").unwrap().browse([]).unwrap();
        let call = ProgressBar {
            domain: Domain::TRUE,
            group_by: "active".to_string(),
            field: "login".to_string(),
            colors: vec!["alice".to_string(), "bob".to_string()],
        };
        let res = users.read_progress_bar(&mut tx, call).await.unwrap();
        value_to_json(&res)
    };
    assert_eq!(ok(&r), expected);

    // 2. Policy refusal
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/read_progress_bar",
        r#"{"domain": [], "group_by": "name", "progress_bar": {"field": "name", "colors": {}}}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusals: invalid field / group_by
    let r_bad_field = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "nope", "progress_bar": {"field": "login", "colors": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_field,
        422,
        "erp.exceptions.ValidationError",
        "Invalid field 'nope' on model 'res.users'",
    );

    // 4. Missing required parameters
    let r_missing_domain = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"group_by": "active", "progress_bar": {"field": "login", "colors": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_domain,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'domain'",
    );

    let r_missing_group_by = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "progress_bar": {"field": "login", "colors": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_group_by,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'group_by'",
    );

    let r_missing_pb = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_pb,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'progress_bar'",
    );

    // 5. Invalid progress_bar parameter type & shape
    let r_invalid_pb_str = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active", "progress_bar": "not a dict"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_invalid_pb_str,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'progress_bar' of res.users.read_progress_bar: expected an object, got string",
    );

    let r_invalid_field = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active", "progress_bar": {"field": 1, "colors": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_invalid_field,
        422,
        "erp.exceptions.ValidationError",
        "argument 'progress_bar' of res.users.read_progress_bar: 'field' must be a string, got integer",
    );

    let r_missing_colors = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active", "progress_bar": {"field": "login"}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_colors,
        422,
        "erp.exceptions.ValidationError",
        "argument 'progress_bar' of res.users.read_progress_bar: 'colors' must be an object, got null",
    );

    // 6. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"domain": [], "group_by": "active", "progress_bar": {"field": "login", "colors": {}}, "extra": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    // 7. Called with ids
    let r_ids = call(
        &h,
        "/json/2/res.users/read_progress_bar",
        r#"{"ids": [1], "domain": [], "group_by": "active", "progress_bar": {"field": "login", "colors": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.read_progress_bar with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_web_name_search() {
    let h = harness().await;

    // 1. Success with result shape and formatting
    let r = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {"display_name": {}}}"#,
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!([{"id": ALICE, "display_name": "alice", "__formatted_display_name": "alice"}])
    );

    // Limits: more matching records than NAME_SEARCH_LIMIT, so the default is what cuts the result
    let seeded = erp_orm::NAME_SEARCH_LIMIT + 20;
    sql(
        &h,
        &format!(
            "INSERT INTO test_rpc_model_b (name, value) SELECT 'nm-' || lpad(g::text, 4, '0'), 1 \
             FROM generate_series(1, {seeded}) g RETURNING id::text"
        ),
    )
    .await;
    let name_search = |extra: &str| {
        format!(r#"{{"name": "nm-", "specification": {{"display_name": {{}}}}{extra}}}"#)
    };
    let count = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/search_count",
        r#"{"domain": [["name", "ilike", "nm-"]]}"#,
        alice(),
    )
    .await);
    assert_eq!(count, json!(seeded), "every seeded record matches");
    // omitted limit: the signature default, equal to passing NAME_SEARCH_LIMIT explicitly
    let r_omitted = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(""),
        alice(),
    )
    .await);
    let r_explicit = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(&format!(r#", "limit": {}"#, erp_orm::NAME_SEARCH_LIMIT)),
        alice(),
    )
    .await);
    assert_eq!(
        r_omitted.as_array().map(Vec::len),
        Some(usize::try_from(erp_orm::NAME_SEARCH_LIMIT).unwrap())
    );
    assert_eq!(r_omitted, r_explicit);
    // an explicit smaller limit
    let r_one = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(r#", "limit": 1"#),
        alice(),
    )
    .await);
    assert_eq!(r_one.as_array().map(Vec::len), Some(1));
    // limit null: no limit, exactly what search_count counts
    let r_unlimited = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(r#", "limit": null"#),
        alice(),
    )
    .await);
    assert_eq!(
        r_unlimited.as_array().map(|l| json!(l.len())),
        Some(count.clone())
    );
    let r_domain_null = ok(&call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(r#", "domain": null, "limit": null"#),
        alice(),
    )
    .await);
    assert_eq!(r_domain_null, r_unlimited);
    let r_operator_null = call(
        &h,
        "/json/2/test_rpc.model_b/web_name_search",
        &name_search(r#", "operator": null"#),
        alice(),
    )
    .await;
    assert_error(
        &r_operator_null,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'operator' of test_rpc.model_b.web_name_search: expected a domain operator, got null",
    );

    // Domain/operator case that changes the result
    let r_eq = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {"login": {}}, "operator": "="}"#,
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r_eq),
        json!([]),
        "operator '=' exact match does not find 'alice' with name 'ali'"
    );

    let r_filtered = call(
        &h,
        "/json/2/res.users/web_name_search",
        &format!(
            r#"{{"name": "ali", "specification": {{"login": {{}}}}, "domain": [["id", "!=", {ALICE}]]}}"#
        ),
        alice(),
    )
    .await;
    assert_eq!(ok(&r_filtered), json!([]), "domain filter excludes alice");

    // R3-S4: web_name_search is a read method, so a read-only key may call it
    let r_reader = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {"display_name": {}}}"#,
        alice_key(scope(7, "reader", ModelAllow::All, true)),
    )
    .await;
    assert_eq!(
        ok(&r_reader),
        json!([{"id": ALICE, "display_name": "alice", "__formatted_display_name": "alice"}])
    );

    // 2. Refused under restricting policy
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/web_name_search",
        r#"{"name": "", "specification": {"name": {}}}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // S2: Field-level access refusal on specification -> 403 AccessError
    let policy = erp_testkit::FieldPolicy::new().forbid("test_fa.item", "forbidden3");
    let h_fa = harness_with_policy(
        &[&erp_test_models::test_field_access::ADDON],
        policy,
        RpcConfig::default(),
        false,
    )
    .await;
    let r_field_access = call(
        &h_fa,
        "/json/2/test_fa.item/web_name_search",
        r#"{"name": "", "specification": {"forbidden3": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_field_access,
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"forbidden3\" on Object For Test Access Right (test_fa.item). Please contact your system administrator.\n\nOperation: read",
    );
    h_fa.cleanup().await.unwrap();

    // 3. Missing required parameters: 'name' is required (R1)
    let r_missing_name = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"specification": {"login": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_name,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'name'",
    );

    let r_missing_spec = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_spec,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'specification'",
    );

    // 4. Invalid parameter types
    let r_bad_limit = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {}, "limit": -1}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_limit,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'limit' of res.users.web_name_search: expected a non-negative integer or null, got -1",
    );

    let r_bad_op = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {}, "operator": "bad_op"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_op,
        422,
        "erp.exceptions.ValidationError",
        "argument 'operator' of res.users.web_name_search: Invalid operator \"bad_op\"",
    );

    // 5. Unexpected argument
    let r_extra = call(
        &h,
        "/json/2/res.users/web_name_search",
        r#"{"name": "ali", "specification": {}, "extra": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_extra,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/res.users/web_name_search",
        &format!(r#"{{"ids": [{ALICE}], "name": "ali", "specification": {{}}}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call res.users.web_name_search with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_web_save_multi() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    let ids = sql(
        &h,
        "INSERT INTO test_web_person (name) VALUES ('p1'), ('p2'), ('p3') RETURNING id::text",
    )
    .await;
    let id1: i64 = ids[0].as_ref().unwrap().parse().unwrap();
    let id2: i64 = ids[1].as_ref().unwrap().parse().unwrap();
    let id3: i64 = ids[2].as_ref().unwrap().parse().unwrap();

    // 1. Success with grouped values: records sharing vals written together
    let r = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(
            r#"{{"ids": [{id1}, {id2}, {id3}], "vals_list": [{{"name": "shared"}}, {{"name": "shared"}}, {{"name": "unique"}}], "specification": {{"name": {{}}}}}}"#
        ),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!([{"id": id1, "name": "shared"}, {"id": id2, "name": "shared"}, {"id": id3, "name": "unique"}])
    );

    // 1b. Grouping through the binder: one `write` per group of equal values (GRP §7 D4). The model's
    // `write` override records each call's batch size and stores `priority + 1`. The SQL is the same
    // with or without grouping (the flush writes one `UPDATE … FROM unnest` per field set), so the
    // write calls are what is counted. The last two entries share their keys but not their values: a
    // wrong merge would give them the same priority.
    let w = sql(
        &h,
        "INSERT INTO test_orm_write_override (priority, body, active) VALUES \
         (0, 'a', true), (0, 'b', true), (0, 'c', true), (0, 'd', true), (0, 'e', true), \
         (0, 'f', true) RETURNING id::text",
    )
    .await;
    let w: Vec<i64> = w
        .iter()
        .map(|id| id.as_ref().unwrap().parse().unwrap())
        .collect();
    erp_test_models::test_orm::WRITE_BATCH_SIZES
        .lock()
        .unwrap()
        .clear();
    // the body is written by hand: the last two entries are equal maps in another key order
    // (`json!` would sort the keys)
    let r_grouped = call(
        &h,
        "/json/2/test_orm.write_override/web_save_multi",
        &format!(
            r#"{{"ids": [{}, {}, {}, {}, {}, {}], "vals_list": [{{"body": "shared"}}, {{"body": "shared"}}, {{"priority": 5}}, {{"priority": 6}}, {{"body": "x", "priority": 3}}, {{"priority": 3, "body": "x"}}], "specification": {{"body": {{}}, "priority": {{}}}}}}"#,
            w[0], w[1], w[2], w[3], w[4], w[5]
        ),
        alice(),
    )
    .await;
    assert_eq!(
        *erp_test_models::test_orm::WRITE_BATCH_SIZES.lock().unwrap(),
        vec![2, 1, 1, 2],
        "equal entries are one write (also in another key order), different ones a write each"
    );
    assert_eq!(
        ok(&r_grouped),
        json!([
            {"id": w[0], "body": "shared", "priority": 0},
            {"id": w[1], "body": "shared", "priority": 0},
            {"id": w[2], "body": "c", "priority": 6},
            {"id": w[3], "body": "d", "priority": 7},
            {"id": w[4], "body": "x", "priority": 4},
            {"id": w[5], "body": "x", "priority": 4},
        ])
    );

    // 2. Mismatched ids and vals_list lengths fails loudly
    let r_mismatch = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(
            r#"{{"ids": [{id1}, {id2}], "vals_list": [{{"name": "a"}}], "specification": {{}}}}"#
        ),
        alice(),
    )
    .await;
    assert_error(
        &r_mismatch,
        422,
        "erp.exceptions.ValidationError",
        "Each record must have a corresponding vals entry.",
    );

    // S1: Batch cap limit (1000 records maximum)
    let too_many_vals: Vec<_> = (0..=erp_orm::MAX_SAVE_MULTI_RECORDS)
        .map(|_| json!({"name": "x"}))
        .collect();
    let too_many_ids: Vec<i64> = (1..=erp_orm::MAX_SAVE_MULTI_RECORDS as i64 + 1).collect();
    let r_capped = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &serde_json::to_string(&json!({
            "ids": too_many_ids,
            "vals_list": too_many_vals,
            "specification": {}
        }))
        .unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_capped,
        422,
        "erp.exceptions.ValidationError",
        "web_save_multi accepts at most 1000 records, got 1001",
    );

    // 3. Refused under restricting policy (Bob may not write test_rpc.model_a)
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/web_save_multi",
        r#"{"ids": [1], "vals_list": [{"name": "x"}], "specification": {}}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you write 'Model A' (test_rpc.model_a) records.",
    );

    // S2: Write binder called with read-only API key -> 403 AccessError
    let reader = || alice_key(scope(7, "reader", ModelAllow::All, true));
    let r_ro = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(r#"{{"ids": [{id1}], "vals_list": [{{"name": "ro"}}], "specification": {{}}}}"#),
        reader(),
    )
    .await;
    assert_error(
        &r_ro,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'test_web.person/web_save_multi' (effect 'write').",
    );

    // S2: Field-level access refusal on specification -> 403 AccessError
    let policy = erp_testkit::FieldPolicy::new().forbid("test_fa.item", "forbidden3");
    let h_fa = harness_with_policy(
        &[&erp_test_models::test_field_access::ADDON],
        policy,
        RpcConfig::default(),
        false,
    )
    .await;
    let r_field_access = call(
        &h_fa,
        "/json/2/test_fa.item/web_save_multi",
        r#"{"ids": [1], "vals_list": [{"val": 10}], "specification": {"forbidden3": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_field_access,
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"forbidden3\" on Object For Test Access Right (test_fa.item). Please contact your system administrator.\n\nOperation: read",
    );
    h_fa.cleanup().await.unwrap();

    // 4. Missing required parameters
    let r_missing_vals = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_missing_vals,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'vals_list'",
    );

    let r_missing_spec = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(r#"{{"ids": [{id1}], "vals_list": [{{}}]}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_missing_spec,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'specification'",
    );

    // 5. Invalid parameter type
    let r_invalid = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(r#"{{"ids": [{id1}], "vals_list": "not_array", "specification": {{}}}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_invalid,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'vals_list' of test_web.person.web_save_multi: expected an array, got string",
    );

    // 6. Unexpected argument
    let r_extra = call(
        &h,
        "/json/2/test_web.person/web_save_multi",
        &format!(
            r#"{{"ids": [{id1}], "vals_list": [{{}}], "specification": {{}}, "extra": true}}"#
        ),
        alice(),
    )
    .await;
    assert_error(
        &r_extra,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_web_resequence() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    let ids = sql(
        &h,
        "INSERT INTO test_web_person (name, sequence) VALUES ('p1', 1), ('p2', 2) RETURNING id::text",
    )
    .await;
    let id1: i64 = ids[0].as_ref().unwrap().parse().unwrap();
    let id2: i64 = ids[1].as_ref().unwrap().parse().unwrap();

    // 1. Success with custom offset
    let r = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(
            r#"{{"ids": [{id2}, {id1}], "specification": {{"sequence": {{}}}}, "offset": 10}}"#
        ),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!([{"id": id2, "sequence": 10}, {"id": id1, "sequence": 11}])
    );

    // 2. Default offset is 0, default field_name is 'sequence'
    let r_default = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}, {id2}], "specification": {{"sequence": {{}}}}}}"#),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r_default),
        json!([{"id": id1, "sequence": 0}, {"id": id2, "sequence": 1}])
    );

    let r_null_field = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}, "field_name": null}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&r_null_field), json!([]));

    // Unknown field_name returns empty list
    let r_unknown = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}, "field_name": "unknown_field"}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&r_unknown), json!([]));

    // 4. R2: offset: null is rejected with 422 (in Python, offset=None raises TypeError)
    let r_null_offset = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}, "offset": null}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_null_offset,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'offset' of test_web.person.web_resequence: expected an integer, got null",
    );

    // R3-S1: every argument is checked before `field_name: null` returns `[]`
    let r_null_field_bad_offset = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}, "field_name": null, "offset": "x"}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_null_field_bad_offset,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'offset' of test_web.person.web_resequence: expected an integer, got string",
    );

    // R3-M2: more than MAX_RESEQUENCE_RECORDS records is refused before any write
    let many: Vec<i64> = sql(
        &h,
        &format!(
            "INSERT INTO test_web_person (name, sequence) SELECT 'm' || g, 0 \
             FROM generate_series(1, {}) g RETURNING id::text",
            erp_orm::MAX_RESEQUENCE_RECORDS + 1
        ),
    )
    .await
    .iter()
    .map(|id| id.as_ref().unwrap().parse().unwrap())
    .collect();
    let r_capped = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &serde_json::to_string(&json!({
            "ids": many,
            "specification": {},
            "offset": 5
        }))
        .unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_capped,
        422,
        "erp.exceptions.ValidationError",
        "web_resequence accepts at most 1000 records, got 1001",
    );
    assert_eq!(
        sql_one(
            &h,
            "SELECT count(*)::text FROM test_web_person WHERE name LIKE 'm%' AND sequence <> 0"
        )
        .await,
        "0",
        "nothing was written"
    );

    // 5. Refused under restricting policy (Bob may not write test_web.person)
    let r_bob = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}}}"#),
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you write 'Person' (test_web.person) records.",
    );

    // S2: Write binder called with read-only API key -> 403 AccessError
    let reader = || alice_key(scope(7, "reader", ModelAllow::All, true));
    let r_ro = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}}}"#),
        reader(),
    )
    .await;
    assert_error(
        &r_ro,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'test_web.person/web_resequence' (effect 'write').",
    );

    // 6. Missing required parameter: specification
    let r_missing = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}]}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'specification'",
    );

    // 7. Unexpected parameter
    let r_extra = call(
        &h,
        "/json/2/test_web.person/web_resequence",
        &format!(r#"{{"ids": [{id1}], "specification": {{}}, "extra": true}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r_extra,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_onchange_batch() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;

    // 1. Success path: R5 model with real onchanges (test_web.discussion), compared with single /onchange
    let r_single1 = call(
        &h,
        "/json/2/test_web.discussion/onchange",
        r#"{"values": {"name": "one warning"}, "field_names": ["name"], "fields_spec": {"name": {}, "note": {}}}"#,
        alice(),
    )
    .await;
    let r_single2 = call(
        &h,
        "/json/2/test_web.discussion/onchange",
        r#"{"values": {"name": "hello"}, "field_names": ["name"], "fields_spec": {"name": {}, "note": {}}}"#,
        alice(),
    )
    .await;
    let r_batch = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": [{"name": "one warning"}, {"name": "hello"}], "field_names": ["name"], "fields_spec": {"name": {}, "note": {}}}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r_batch), json!([ok(&r_single1), ok(&r_single2)]));

    // S1: Batch cap limit (1000 entries maximum)
    let too_many_values: Vec<_> = (0..=erp_orm::MAX_ONCHANGE_BATCH_ENTRIES)
        .map(|_| json!({"name": "x"}))
        .collect();
    let r_capped = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        &serde_json::to_string(&json!({
            "values_list": too_many_values,
            "field_names": ["name"],
            "fields_spec": {"name": {}}
        }))
        .unwrap(),
        alice(),
    )
    .await;
    assert_error(
        &r_capped,
        422,
        "erp.exceptions.ValidationError",
        "onchange_batch accepts at most 1000 values entries, got 1001",
    );

    // 2. Refused when called with ids
    let r_ids = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        &format!(
            r#"{{"ids": [{ALICE}], "values_list": [], "field_names": [], "fields_spec": {{}}}}"#
        ),
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call test_web.discussion.onchange_batch with ids",
    );

    // 3. Refused under restricting policy (Bob may not create res.users)
    let r_bob = call(
        &h,
        "/json/2/res.users/onchange_batch",
        r#"{"values_list": [{"name": "Bob New"}], "field_names": [], "fields_spec": {}}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you create 'User' (res.users) records.",
    );

    // S2: Write binder called with read-only API key -> 403 AccessError
    let reader = || alice_key(scope(7, "reader", ModelAllow::All, true));
    let r_ro = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": [{"name": "hello"}], "field_names": ["name"], "fields_spec": {}}"#,
        reader(),
    )
    .await;
    assert_error(
        &r_ro,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'test_web.discussion/onchange_batch' (effect 'write').",
    );

    // S2: Field-level access refusal on fields_spec -> 403 AccessError
    let policy = erp_testkit::FieldPolicy::new().forbid("test_fa.item", "forbidden3");
    let h_fa = harness_with_policy(
        &[&erp_test_models::test_field_access::ADDON],
        policy,
        RpcConfig::default(),
        false,
    )
    .await;
    let r_field_access = call(
        &h_fa,
        "/json/2/test_fa.item/onchange_batch",
        r#"{"values_list": [{"val": 10}], "field_names": ["val"], "fields_spec": {"forbidden3": {}}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_field_access,
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"forbidden3\" on Object For Test Access Right (test_fa.item). Please contact your system administrator.\n\nOperation: read",
    );
    h_fa.cleanup().await.unwrap();

    // 4. Missing required parameters
    let r_missing_vals = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"field_names": [], "fields_spec": {}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_vals,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'values_list'",
    );

    let r_missing_fields = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": [], "fields_spec": {}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_fields,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'field_names'",
    );

    let r_missing_spec = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": [], "field_names": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing_spec,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'fields_spec'",
    );

    // 5. Invalid parameter type
    let r_invalid = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": "not_array", "field_names": [], "fields_spec": {}}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_invalid,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'values_list' of test_web.discussion.onchange_batch: expected an array, got string",
    );

    // 6. Unexpected argument
    let r_extra = call(
        &h,
        "/json/2/test_web.discussion/onchange_batch",
        r#"{"values_list": [], "field_names": [], "fields_spec": {}, "extra": 1}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_extra,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'extra'",
    );

    h.cleanup().await.unwrap();
}

/// Every context key the data loader sets is in the RPC's server-only `install_` namespace, or
/// in its listed keys (review A4-R1, A5-R1): the producers' constants
/// (`load_state::KEYS`, which the loader's writes use), not a copy of them.
#[test]
fn the_loaders_context_keys_are_in_the_server_only_namespace() {
    use erp_data::load::load_state;
    let refused = |key: &str| {
        key.starts_with(erp_rpc::SERVER_ONLY_CONTEXT_PREFIX)
            || erp_rpc::SERVER_ONLY_CONTEXT_KEYS.contains(&key)
    };
    for key in load_state::KEYS {
        assert!(refused(key), "{key}");
    }
}

#[tokio::test]
async fn json2_search_panel_select_range() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;

    // 1. Success path
    let r = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_range",
        r#"{"field_name": "folder_id"}"#,
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!({
            "parent_field": "parent_name_id",
            "values": []
        })
    );

    // 2. Policy refusal
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/search_panel_select_range",
        r#"{"field_name": "name"}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusal: unsupported field type
    let r_bad_type = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_range",
        r#"{"field_name": "name"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_type,
        422,
        "erp.exceptions.UserError",
        "Only types many2one, selection, many2many are supported for category (found type char)",
    );

    // 4. Missing required parameter: field_name
    let r_missing = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_range",
        r#"{"enable_counters": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'field_name'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_range",
        r#"{"field_name": "folder_id", "bogus": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'bogus'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_range",
        r#"{"ids": [1], "field_name": "folder_id"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call test_search_panel.source_model.search_panel_select_range with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_search_panel_select_multi_range() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;

    // 1. Success path
    let r = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_multi_range",
        r#"{"field_name": "tag_id"}"#,
        alice(),
    )
    .await;
    assert_eq!(
        ok(&r),
        json!({
            "values": []
        })
    );

    // 2. Policy refusal
    let r_bob = call(
        &h,
        "/json/2/test_rpc.model_a/search_panel_select_multi_range",
        r#"{"field_name": "name"}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you read 'Model A' (test_rpc.model_a) records.",
    );

    // 3. Validation refusal: unsupported field type
    let r_bad_type = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_multi_range",
        r#"{"field_name": "name"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_bad_type,
        422,
        "erp.exceptions.UserError",
        "Only types many2one, many2many, selection are supported for filter (found type char)",
    );

    // 4. Missing required parameter: field_name
    let r_missing = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_multi_range",
        r#"{"enable_counters": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'field_name'",
    );

    // 5. Unexpected parameter
    let r_unexpected = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_multi_range",
        r#"{"field_name": "tag_id", "bogus": true}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_unexpected,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'bogus'",
    );

    // 6. Called with ids
    let r_ids = call(
        &h,
        "/json/2/test_search_panel.source_model/search_panel_select_multi_range",
        r#"{"ids": [1], "field_name": "tag_id"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call test_search_panel.source_model.search_panel_select_multi_range with ids",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_search_panel_limit_binding() {
    let h = harness_with(
        &[&erp_test_models::test_web::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    for method in [
        "search_panel_select_range",
        "search_panel_select_multi_range",
    ] {
        let path = format!("/json/2/test_search_panel.source_model/{method}");
        for limit in [
            None,
            Some(json!(null)),
            Some(json!(false)),
            Some(json!(0)),
            Some(json!(3)),
        ] {
            let mut body = json!({"field_name": "state", "expand": true});
            if let Some(limit) = limit {
                body["limit"] = limit;
            }
            let response = call(
                &h,
                &path,
                &body.to_string(),
                alice_key(scope(7, "reader", ModelAllow::All, true)),
            )
            .await;
            assert_eq!(
                ok(&response)["values"],
                json!([{"id": "a", "display_name": "A"}, {"id": "b", "display_name": "B"}]),
                "{method}: {body}",
            );
        }
        let limited = call(
            &h,
            &path,
            r#"{"field_name": "state", "expand": true, "limit": 1}"#,
            alice(),
        )
        .await;
        assert_eq!(
            ok(&limited),
            json!({"error_msg": "Too many items to display."})
        );
        for (limit, got) in [
            (json!(-1), "-1"),
            (json!(true), "boolean"),
            (json!("1"), "string"),
            (json!(1.5), "number"),
            (json!([]), "array"),
            (json!({}), "object"),
        ] {
            let response = call(
                &h,
                &path,
                &json!({"field_name": "state", "limit": limit}).to_string(),
                alice(),
            )
            .await;
            assert_error(
                &response,
                422,
                "werkzeug.exceptions.UnprocessableEntity",
                &format!(
                    "argument 'limit' of test_search_panel.source_model.{method}: expected a non-negative integer, false or null, got {got}"
                ),
            );
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_copy_and_copy_data() {
    let h = harness().await;
    let created = call(
        &h,
        "/json/2/test_orm.partner/create",
        r#"{"vals_list": [{"name": "Starship", "email": "ship@spacex.com"}]}"#,
        alice(),
    )
    .await;
    let id = ok(&created)[0].as_i64().unwrap();

    // 1. copy_data without default
    let cd = call(
        &h,
        "/json/2/test_orm.partner/copy_data",
        &format!(r#"{{"ids": [{id}]}}"#),
        alice(),
    )
    .await;
    let cd_res = ok(&cd);
    assert_eq!(cd_res[0]["name"], "Starship");
    assert_eq!(cd_res[0]["email"], "ship@spacex.com");

    // 2. copy_data with default
    let cd_def = call(
        &h,
        "/json/2/test_orm.partner/copy_data",
        &format!(r#"{{"ids": [{id}], "default": {{"name": "Falcon"}}}}"#),
        alice(),
    )
    .await;
    let cd_def_res = ok(&cd_def);
    assert_eq!(cd_def_res[0]["name"], "Falcon");
    assert_eq!(cd_def_res[0]["email"], "ship@spacex.com");

    // 3. copy without default
    let c = call(
        &h,
        "/json/2/test_orm.partner/copy",
        &format!(r#"{{"ids": [{id}]}}"#),
        alice(),
    )
    .await;
    let new_id = ok(&c)[0].as_i64().unwrap();
    assert_ne!(id, new_id);

    let read = call(
        &h,
        "/json/2/test_orm.partner/read",
        &format!(r#"{{"ids": [{new_id}], "fields": ["name", "email"]}}"#),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&read),
        json!([{"id": new_id, "name": "Starship", "email": "ship@spacex.com"}])
    );

    // 4. copy with default
    let c_def = call(
        &h,
        "/json/2/test_orm.partner/copy",
        &format!(r#"{{"ids": [{id}], "default": {{"name": "Dragon"}}}}"#),
        alice(),
    )
    .await;
    let new_id2 = ok(&c_def)[0].as_i64().unwrap();
    let read2 = call(
        &h,
        "/json/2/test_orm.partner/read",
        &format!(r#"{{"ids": [{new_id2}], "fields": ["name", "email"]}}"#),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&read2),
        json!([{"id": new_id2, "name": "Dragon", "email": "ship@spacex.com"}])
    );

    // 5. Bad arguments (invalid ids)
    let bad = call(
        &h,
        "/json/2/test_orm.partner/copy",
        r#"{"ids": "invalid"}"#,
        alice(),
    )
    .await;
    assert_error(
        &bad,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "ids must be a positive integer or a list of positive integers, got string",
    );

    let bad_cd = call(
        &h,
        "/json/2/test_orm.partner/copy_data",
        r#"{"ids": "invalid"}"#,
        alice(),
    )
    .await;
    assert_error(
        &bad_cd,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "ids must be a positive integer or a list of positive integers, got string",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_name_create() {
    let h = harness().await;

    // 1. Success path on test_orm.partner (has name as _rec_name)
    let r = call(
        &h,
        "/json/2/test_orm.partner/name_create",
        r#"{"name": "Heavy Falcon"}"#,
        alice(),
    )
    .await;
    let res = ok(&r);
    let id = res[0].as_i64().unwrap();
    assert_eq!(res[1], "Heavy Falcon");

    let read = call(
        &h,
        "/json/2/test_orm.partner/read",
        &format!(r#"{{"ids": [{id}], "fields": ["display_name"]}}"#),
        alice(),
    )
    .await;
    assert_eq!(
        ok(&read),
        json!([{"id": id, "display_name": "Heavy Falcon"}])
    );

    // 2. Refuses ids parameter (Receiver is Model)
    let bad_ids = call(
        &h,
        "/json/2/test_orm.partner/name_create",
        r#"{"ids": [1], "name": "Fail"}"#,
        alice(),
    )
    .await;
    assert_error(
        &bad_ids,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call test_orm.partner.name_create with ids",
    );

    // 3. Missing name parameter
    let missing = call(&h, "/json/2/test_orm.partner/name_create", "{}", alice()).await;
    assert_error(
        &missing,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'name'",
    );

    // 4. Model without _rec_name (test_orm.noname)
    let no_rec = call(
        &h,
        "/json/2/test_orm.noname/name_create",
        r#"{"name": "NoRec"}"#,
        alice(),
    )
    .await;
    assert_error(
        &no_rec,
        500,
        "erp.programming",
        "Cannot name_create on test_orm.noname",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_action_archive_and_unarchive() {
    let h = harness().await;

    let created = call(
        &h,
        "/json/2/test_orm.partner/create",
        r#"{"vals_list": [{"name": "Archivable"}]}"#,
        alice(),
    )
    .await;
    let id = ok(&created)[0].as_i64().unwrap();

    // 1. archive
    let arc = call(
        &h,
        "/json/2/test_orm.partner/action_archive",
        &format!(r#"{{"ids": [{id}]}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&arc), serde_json::Value::Null);

    let read = call(
        &h,
        "/json/2/test_orm.partner/read",
        &format!(r#"{{"ids": [{id}], "fields": ["active"]}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&read), json!([{"id": id, "active": false}]));

    // 2. unarchive
    let unarc = call(
        &h,
        "/json/2/test_orm.partner/action_unarchive",
        &format!(r#"{{"ids": [{id}]}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&unarc), serde_json::Value::Null);

    let read2 = call(
        &h,
        "/json/2/test_orm.partner/read",
        &format!(r#"{{"ids": [{id}], "fields": ["active"]}}"#),
        alice(),
    )
    .await;
    assert_eq!(ok(&read2), json!([{"id": id, "active": true}]));

    // 3. Bad arguments (invalid ids)
    let bad = call(
        &h,
        "/json/2/test_orm.partner/action_archive",
        r#"{"ids": "invalid"}"#,
        alice(),
    )
    .await;
    assert_error(
        &bad,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "ids must be a positive integer or a list of positive integers, got string",
    );

    // 4. Model without active field (test_rpc.model_b)
    let no_active = call(
        &h,
        "/json/2/test_rpc.model_b/action_archive",
        r#"{"ids": [1]}"#,
        alice(),
    )
    .await;
    assert_error(
        &no_active,
        500,
        "erp.programming",
        "No 'active' field on model test_rpc.model_b",
    );

    let no_active_un = call(
        &h,
        "/json/2/test_rpc.model_b/action_unarchive",
        r#"{"ids": [1]}"#,
        alice(),
    )
    .await;
    assert_error(
        &no_active_un,
        500,
        "erp.programming",
        "No 'active' field on model test_rpc.model_b",
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json2_copy_archive_access_refusals() {
    let h = harness().await;

    // Alice creates a partner record first
    let created = call(
        &h,
        "/json/2/test_orm.partner/create",
        r#"{"vals_list": [{"name": "Acme Partner"}]}"#,
        alice(),
    )
    .await;
    let rec_id = ok(&created)[0].as_i64().unwrap();

    // Bob may only read
    let r_copy = call(
        &h,
        "/json/2/test_orm.partner/copy",
        &format!(r#"{{"ids": [{rec_id}]}}"#),
        user(BOB),
    )
    .await;
    assert_error(
        &r_copy,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you create 'Test ORM Partner' (test_orm.partner) records.",
    );

    let r_arc = call(
        &h,
        "/json/2/test_orm.partner/action_archive",
        &format!(r#"{{"ids": [{rec_id}]}}"#),
        user(BOB),
    )
    .await;
    assert_error(
        &r_arc,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you write 'Test ORM Partner' (test_orm.partner) records.",
    );

    let r_nc = call(
        &h,
        "/json/2/test_orm.partner/name_create",
        r#"{"name": "NoBob"}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_nc,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you create 'Test ORM Partner' (test_orm.partner) records.",
    );

    // Read-only API key
    let reader = || alice_key(scope(8, "reader", ModelAllow::All, true));
    for (method, body) in [
        ("copy", format!(r#"{{"ids": [{rec_id}]}}"#)),
        ("copy_data", format!(r#"{{"ids": [{rec_id}]}}"#)),
        ("name_create", r#"{"name": "X"}"#.to_string()),
        ("action_archive", format!(r#"{{"ids": [{rec_id}]}}"#)),
        ("action_unarchive", format!(r#"{{"ids": [{rec_id}]}}"#)),
    ] {
        let r = call(
            &h,
            &format!("/json/2/test_orm.partner/{method}"),
            &body,
            reader(),
        )
        .await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "The API key 'reader' is read-only: it may not call 'test_orm.partner/{method}' (effect 'write')."
            ),
        );
    }

    h.cleanup().await.unwrap();
}

/// COPY bindings reject wrong types and unknown keywords before invoking the ORM.
#[tokio::test]
async fn json2_copy_methods_bad_parameters() {
    let h = harness().await;
    for method in ["copy", "copy_data", "action_archive", "action_unarchive"] {
        let r = call(
            &h,
            &format!("/json/2/test_orm.partner/{method}"),
            r#"{"ids": "invalid"}"#,
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "ids must be a positive integer or a list of positive integers, got string",
        );
    }
    for method in ["copy", "copy_data"] {
        let r = call(
            &h,
            &format!("/json/2/test_orm.partner/{method}"),
            r#"{"ids": [], "default": []}"#,
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            &format!(
                "argument 'default' of test_orm.partner.{method}: expected an object, got array"
            ),
        );
    }
    let r = call(
        &h,
        "/json/2/test_orm.partner/name_create",
        r#"{"name": 7}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'name' of test_orm.partner.name_create: expected a string, got integer",
    );
    for (method, body) in [
        ("copy", r#"{"ids": [], "extra": true}"#),
        ("copy_data", r#"{"ids": [], "extra": true}"#),
        ("name_create", r#"{"name": "X", "extra": true}"#),
        ("action_archive", r#"{"ids": [], "extra": true}"#),
        ("action_unarchive", r#"{"ids": [], "extra": true}"#),
    ] {
        let r = call(
            &h,
            &format!("/json/2/test_orm.partner/{method}"),
            body,
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "got an unexpected keyword argument 'extra'",
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_rpc_x2many_short_commands_res_users() {
    let h = harness_with(
        &[&erp_test_models::test_rel::ADDON],
        RpcConfig::default(),
        false,
    )
    .await;
    let b = sql(
        &h,
        "INSERT INTO test_rel_base (name) VALUES ('b1') RETURNING id::text",
    )
    .await;
    let bid: i64 = b[0].as_ref().unwrap().parse().unwrap();
    let t = sql(
        &h,
        "INSERT INTO test_rel_tag (name) VALUES ('t1') RETURNING id::text",
    )
    .await;
    let tid: i64 = t[0].as_ref().unwrap().parse().unwrap();

    // 1. Valid short command [[4, id]] on write succeeds with 200
    let r = call(
        &h,
        "/json/2/test_rel.base/write",
        &format!(r#"{{"ids": [{bid}], "vals": {{"tag_ids": [[4, {tid}]]}}}}"#),
        alice(),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "status: {}, body: {}",
        r.status(),
        String::from_utf8_lossy(r.body())
    );

    // 2. Invalid short command [[4]] fails with 422
    let r = call(
        &h,
        "/json/2/test_rel.base/write",
        &format!(r#"{{"ids": [{bid}], "vals": {{"tag_ids": [[4]]}}}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "Wrong value for test_rel.base.tag_ids: [4]",
    );

    // 3. Invalid over-long command [[4, 1, 0, 0]] fails with 422
    let r = call(
        &h,
        "/json/2/test_rel.base/write",
        &format!(r#"{{"ids": [{bid}], "vals": {{"tag_ids": [[4, {tid}, 0, 0]]}}}}"#),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        &format!("Wrong value for test_rel.base.tag_ids: [4, {tid}, 0, 0]"),
    );

    h.cleanup().await.unwrap();
}
