//! Search-panel rejection and authorization through the actual native HTTP server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration assertions and shared fixture helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/search_panel_fixture.rs"]
mod search_panel_fixture;

use native_http::{Reply, request};
use search_panel_fixture::PanelFixture;
use serde_json::{Value, json};

fn error(reply: Reply, status: u16, kind: &str, message: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["name"], kind);
    assert_eq!(body["message"], message);
    assert!(
        body.get("values").is_none(),
        "error exposed successful values"
    );
}

fn success(reply: Reply) -> Value {
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

const RANGE: &str = "search_panel_select_range";
const MULTI: &str = "search_panel_select_multi_range";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_inputs_and_fields_fail_precisely() {
    let fixture = PanelFixture::start().await;
    for method in [RANGE, MULTI] {
        for (body, message) in [
            (
                json!({}),
                "missing a required argument: 'field_name'".to_owned(),
            ),
            (
                json!({"field_name":"state", "bogus":true}),
                "got an unexpected keyword argument 'bogus'".to_owned(),
            ),
            (
                json!({"field_name":"state", "ids":[1]}),
                format!("cannot call panel_http.item.{method} with ids"),
            ),
        ] {
            error(
                fixture.session(method, body),
                422,
                "werkzeug.exceptions.UnprocessableEntity",
                &message,
            );
        }
        for (limit, got) in [
            (json!(-1), "-1"),
            (json!(true), "boolean"),
            (json!("1"), "string"),
            (json!(1.5), "number"),
            (json!([]), "array"),
            (json!({}), "object"),
        ] {
            error(
                fixture.session(method, json!({"field_name":"state", "limit":limit})),
                422,
                "werkzeug.exceptions.UnprocessableEntity",
                &format!(
                    "argument 'limit' of panel_http.item.{method}: expected a non-negative integer, false or null, got {got}"
                ),
            );
        }
        error(
            fixture.session(method, json!({"field_name":"missing"})),
            422,
            "erp.exceptions.ValidationError",
            "Field 'missing' does not exist on model 'panel_http.item'",
        );
        let role = if method == RANGE {
            "category"
        } else {
            "filter"
        };
        let types = if method == RANGE {
            "many2one, selection, many2many"
        } else {
            "many2one, many2many, selection"
        };
        error(
            fixture.session(method, json!({"field_name":"name"})),
            422,
            "erp.exceptions.UserError",
            &format!("Only types {types} are supported for {role} (found type char)"),
        );
        error(
            fixture.user_session(method, json!({"field_name":"secret_state", "expand":true})),
            403,
            "erp.exceptions.AccessError",
            "You do not have enough rights to access the field \"secret_state\" on Panel Item (panel_http.item). Please contact your system administrator.\n\nOperation: read",
        );
    }
    error(
        fixture.user_session(
            MULTI,
            json!({"field_name":"tag_ids", "group_by":"secret_family", "expand":true}),
        ),
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"secret_family\" on Panel Tag (panel_http.tag). Please contact your system administrator.\n\nOperation: read",
    );
    for (group_domain, message) in [
        (
            json!([]),
            "search_panel_select_multi_range: group_domain must be a map of JSON-encoded group ids to domains",
        ),
        (
            json!({"null":[]}),
            "search_panel_select_multi_range: group ids must be JSON scalars other than null",
        ),
        (
            json!({"[]":[]}),
            "search_panel_select_multi_range: group ids must be JSON scalars other than null",
        ),
        (
            json!({"\"é\"":[], "\"\\u00e9\"":[]}),
            "search_panel_select_multi_range: duplicate JSON group id \"é\"",
        ),
    ] {
        error(
            fixture.session(
                MULTI,
                json!({"field_name":"tag_ids", "group_by":"family", "group_domain":group_domain}),
            ),
            422,
            "erp.exceptions.ValidationError",
            message,
        );
    }
    let domains: serde_json::Map<String, Value> = (0..65)
        .map(|index| {
            (
                serde_json::to_string(&format!("g{index}")).unwrap(),
                json!([]),
            )
        })
        .collect();
    error(
        fixture.session(
            MULTI,
            json!({"field_name":"tag_ids", "group_by":"family", "group_domain":domains}),
        ),
        422,
        "erp.exceptions.ValidationError",
        "search_panel_select_multi_range: group_domain accepts at most 64 entries, got 65",
    );
    let domains: serde_json::Map<String, Value> = (0..64)
        .map(|index| {
            (
                serde_json::to_string(&format!("g{index}")).unwrap(),
                json!([]),
            )
        })
        .collect();
    let admitted = success(fixture.session(
        MULTI,
        json!({"field_name":"tag_ids", "group_by":"family", "group_domain":domains}),
    ));
    assert!(admitted["values"].is_array());
    // A refused request must not poison the next ordinary request on this serving process.
    let healthy =
        success(fixture.user_session(RANGE, json!({"field_name":"state", "expand":true})));
    assert!(healthy["values"].is_array());
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_values_and_counts_follow_the_reader() {
    let fixture = PanelFixture::start().await;
    // A denied source contributes to readable buckets for the admin. Bucket filtering alone
    // cannot make this probe pass if the counter query forgets source-record authorization.
    let overlap: i64 = fixture.native.client.query_one(
        "INSERT INTO panel_http_item(name,state,visible,company_id,category_id) VALUES('Denied overlap','beta',false,$1,$2) RETURNING id",
        &[&fixture.ids.company, &fixture.ids.child]).await.unwrap().get(0);
    fixture
        .native
        .client
        .execute(
            "INSERT INTO panel_http_item_tag_rel(item_id,tag_id) VALUES($1,$2)",
            &[&overlap, &fixture.ids.cool_tag],
        )
        .await
        .unwrap();
    let cases = [
        (
            RANGE,
            "category_id",
            fixture.ids.hidden_category,
            vec![
                (fixture.ids.root, 2),
                (fixture.ids.child, 1),
                (fixture.ids.other, 1),
            ],
            fixture.ids.child,
            2,
        ),
        (
            MULTI,
            "tag_ids",
            fixture.ids.hidden_tag,
            vec![
                (fixture.ids.cool_tag, 2),
                (fixture.ids.warm_tag, 2),
                (fixture.ids.unused_tag, 0),
            ],
            fixture.ids.cool_tag,
            3,
        ),
    ];
    let mut before_relational = Vec::new();
    for (method, field, hidden, expected_counts, overlap_bucket, admin_count) in &cases {
        let body = json!({"field_name":field, "expand":true, "enable_counters":true});
        let admin = success(fixture.session(method, body.clone()));
        let user = success(fixture.user_session(method, body.clone()));
        let key = fixture.key(fixture.user_id).await;
        let reader = success(fixture.bearer(method, body, &key));
        assert_eq!(
            user, reader,
            "native user API-key authority differs from its subject"
        );
        let admin_values = admin["values"].as_array().unwrap();
        assert!(admin_values.iter().any(|value| value["id"] == *hidden));
        assert_eq!(
            admin_values
                .iter()
                .find(|value| value["id"] == *overlap_bucket)
                .unwrap()["__count"],
            *admin_count
        );
        let actual: std::collections::BTreeMap<i64, i64> = user["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| {
                (
                    value["id"].as_i64().unwrap(),
                    value["__count"].as_i64().unwrap(),
                )
            })
            .collect();
        let expected: std::collections::BTreeMap<i64, i64> =
            expected_counts.iter().copied().collect();
        assert_eq!(actual, expected, "authorized count mismatch: {user}");
        assert_eq!(
            user["values"].as_array().unwrap().len(),
            expected_counts.len()
        );
        assert!(!actual.contains_key(hidden), "hidden comodel value leaked");
        before_relational.push(user);
    }
    // A correctly authenticated caller is still subject to every declared domain.
    for method in [RANGE, MULTI] {
        let empty = success(fixture.user_session(
            method,
            json!({"field_name":"state", "search_domain":[["id","=",-1]], "enable_counters":true}),
        ));
        assert_eq!(empty["values"], json!([]));
    }
    // A readable source row in another company must not enter either selection counter.
    let before: Vec<_> = [RANGE, MULTI]
        .into_iter()
        .map(|method| {
            success(fixture.user_session(
                method,
                json!({"field_name":"state", "expand":true, "enable_counters":true}),
            ))
        })
        .collect();
    let foreign: i64 = fixture.native.client.query_one(
        "INSERT INTO res_company(name,currency_id) SELECT 'Panel foreign company',currency_id FROM res_company WHERE id=$1 RETURNING id",
        &[&fixture.ids.company]).await.unwrap().get(0);
    let foreign_item: i64 = fixture.native.client.query_one(
        "INSERT INTO panel_http_item(name,state,visible,company_id,category_id) VALUES('Foreign visible item','alpha',true,$1,$2) RETURNING id",
        &[&foreign, &fixture.ids.child]).await.unwrap().get(0);
    for (index, method) in [RANGE, MULTI].into_iter().enumerate() {
        let after = success(fixture.user_session(
            method,
            json!({"field_name":"state", "expand":true, "enable_counters":true}),
        ));
        assert_eq!(
            after, before[index],
            "another company's source row entered counts"
        );
        let foreign_only = success(fixture.user_session(method,
            json!({"field_name":"state", "search_domain":[["id","=",foreign_item]], "enable_counters":true})));
        assert_eq!(foreign_only["values"], json!([]));
    }
    fixture
        .native
        .client
        .execute(
            "INSERT INTO panel_http_item_tag_rel(item_id,tag_id) VALUES($1,$2)",
            &[&foreign_item, &fixture.ids.cool_tag],
        )
        .await
        .unwrap();
    for (index, (method, field, ..)) in cases.iter().enumerate() {
        let after = success(fixture.user_session(
            method,
            json!({"field_name":field, "expand":true, "enable_counters":true}),
        ));
        assert_eq!(
            after, before_relational[index],
            "foreign-company row entered relational counters"
        );
    }
    // Missing browser credentials are rejected at native ingress, before RPC execution.
    let reply = request(
        fixture.native.port,
        "POST",
        "/erp/json/2/panel_http.item/search_panel_select_range",
        &[
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        r#"{"field_name":"state"}"#,
    );
    error(
        reply,
        401,
        "werkzeug.exceptions.Unauthorized",
        "User not authenticated, use an API Key with a Bearer Authorization header.",
    );
    error(
        fixture.bearer(RANGE, json!({"field_name":"state"}), "Bearer invalid"),
        401,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_state_survives_server_process_loss() {
    let mut fixture = PanelFixture::start().await;
    let peer = fixture.native.peer();
    let body = json!({"field_name":"state", "expand":true, "enable_counters":true});
    let counts = |value: &Value| {
        value["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["__count"].as_i64().unwrap())
            .sum::<i64>()
    };
    for method in [RANGE, MULTI] {
        let path = format!("/erp/json/2/panel_http.item/{method}");
        let initial = success(fixture.user_session(method, body.clone()));
        assert_eq!(
            initial,
            success(native_http::json_post(
                peer.port,
                &fixture.user_cookies,
                &path,
                &body.to_string()
            ))
        );
        // Only the three visible fixture items contribute to this subject's counts.
        assert_eq!(
            counts(&initial),
            i64::try_from(fixture.ids.items.len() - 1).unwrap()
        );
    }
    fixture
        .native
        .client
        .execute(
            "UPDATE panel_http_item SET visible=false WHERE visible=true",
            &[],
        )
        .await
        .unwrap();
    for method in [RANGE, MULTI] {
        let path = format!("/erp/json/2/panel_http.item/{method}");
        let changed = success(fixture.user_session(method, body.clone()));
        assert_eq!(counts(&changed), 0);
        assert_eq!(
            changed,
            success(native_http::json_post(
                peer.port,
                &fixture.user_cookies,
                &path,
                &body.to_string()
            ))
        );
    }
    // Real policy triggers invalidate both warm processes; no test-only refresh is used.
    fixture.native.client.execute(
        "UPDATE ir_access SET active=false WHERE group_id IS NOT NULL AND model_id=(SELECT id FROM ir_model WHERE model='panel_http.item')", &[])
        .await.unwrap();
    let message = "You are not allowed to access 'Panel Item' (panel_http.item) records.\n\nNo group currently allows this operation.\n\nContact your administrator to request access if necessary.";
    for method in [RANGE, MULTI] {
        let path = format!("/erp/json/2/panel_http.item/{method}");
        error(
            fixture.user_session(method, body.clone()),
            403,
            "erp.exceptions.AccessError",
            message,
        );
        error(
            native_http::json_post(peer.port, &fixture.user_cookies, &path, &body.to_string()),
            403,
            "erp.exceptions.AccessError",
            message,
        );
    }
    fixture.native.server.stop();
    for method in [RANGE, MULTI] {
        let path = format!("/erp/json/2/panel_http.item/{method}");
        error(
            native_http::json_post(peer.port, &fixture.user_cookies, &path, &body.to_string()),
            403,
            "erp.exceptions.AccessError",
            message,
        );
    }
    // B can also serve committed data after A is gone, with the new durable policy.
    fixture.native.client.execute(
        "UPDATE ir_access SET active=true WHERE group_id IS NOT NULL AND model_id=(SELECT id FROM ir_model WHERE model='panel_http.item')", &[])
        .await.unwrap();
    fixture
        .native
        .client
        .execute(
            "UPDATE panel_http_item SET visible=true WHERE id=$1",
            &[&fixture.ids.items[0]],
        )
        .await
        .unwrap();
    for method in [RANGE, MULTI] {
        let path = format!("/erp/json/2/panel_http.item/{method}");
        let survived = success(native_http::json_post(
            peer.port,
            &fixture.user_cookies,
            &path,
            &body.to_string(),
        ));
        assert_eq!(counts(&survived), 1);
    }
    drop(peer);
    fixture.cleanup().await;
}
