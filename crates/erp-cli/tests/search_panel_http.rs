//! Search-panel data protocols through the actual native server and installed addon.
//! This covers server responses; mounted client invocation and badges remain with SEARCHVIEW.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration assertions and shared native fixture helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/search_panel_fixture.rs"]
mod search_panel_fixture;
use search_panel_fixture::{MULTI, PanelFixture, RANGE, success};
use serde_json::{Value, json};

fn selections(alpha: i64, beta: i64, category: bool) -> Value {
    let mut result = json!({"values":[
        {"id":"alpha","display_name":"Alpha","__count":alpha},
        {"id":"beta","display_name":"Beta","__count":beta},
    ]});
    if category {
        result["parent_field"] = json!(false);
    }
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_ranges_preserve_values_counters_and_display_limits() {
    let fixture = PanelFixture::start().await;
    let ids = &fixture.ids;
    let category = success(&fixture.session(
        RANGE,
        json!({
            "field_name":"category_id", "expand":true, "enable_counters":true,
        }),
    ));
    assert_eq!(
        category,
        json!({"parent_field":"parent_id", "values":[
            {"id":ids.root,"display_name":"Root","parent_id":false,"__count":2},
            {"id":ids.child,"display_name":"Child","parent_id":ids.root,"__count":1},
            {"id":ids.other,"display_name":"Other","parent_id":false,"__count":1},
            {"id":ids.hidden_category,"display_name":"Hidden category","parent_id":false,"__count":1},
        ]})
    );

    // A non-expanded category keeps an otherwise unselected ancestor to close the tree.
    let descendants = success(&fixture.session(
        RANGE,
        json!({
            "field_name":"category_id", "search_domain":[["id","=",ids.items[0]]],
            "expand":false, "enable_counters":true,
        }),
    ));
    assert_eq!(
        descendants,
        json!({"parent_field":"parent_id", "values":[
            {"id":ids.root,"display_name":"Root","parent_id":false,"__count":1},
            {"id":ids.child,"display_name":"Child","parent_id":ids.root,"__count":1},
        ]})
    );

    for method in [RANGE, MULTI] {
        let expected = selections(2, 2, method == RANGE);
        let mut body = json!({"field_name":"state", "expand":true, "enable_counters":true});
        assert_eq!(success(&fixture.session(method, body.clone())), expected);
        // False/zero/null are display-unlimited; safety bounds remain independently enforced.
        for limit in [json!(false), json!(0), Value::Null] {
            body["limit"] = limit;
            assert_eq!(success(&fixture.session(method, body.clone())), expected);
        }
        body["limit"] = json!(2);
        assert_eq!(
            success(&fixture.session(method, body.clone())),
            json!({"error_msg":"Too many items to display."})
        );
        body["limit"] = json!(3);
        assert_eq!(success(&fixture.session(method, body.clone())), expected);
        body["search_domain"] = json!([["id", "=", -1]]);
        assert_eq!(
            success(&fixture.session(method, body)),
            selections(0, 0, method == RANGE)
        );
    }

    let grouped = success(&fixture.session(
        MULTI,
        json!({
            "field_name":"tag_ids", "group_by":"family", "expand":true, "enable_counters":true,
        }),
    ));
    assert_eq!(
        grouped,
        json!({"values":[
            {"id":ids.cool_tag,"display_name":"Cool tag","group_id":"cool","group_name":"Cool","__count":2},
            {"id":ids.warm_tag,"display_name":"Warm tag","group_id":"warm","group_name":"Warm","__count":2},
            {"id":ids.unused_tag,"display_name":"Unused tag","group_id":"cool","group_name":"Cool","__count":0},
            {"id":ids.hidden_tag,"display_name":"Hidden tag","group_id":"warm","group_name":"Warm","__count":1},
        ]})
    );
    let filtered = success(&fixture.session(
        MULTI,
        json!({
            "field_name":"tag_ids", "group_by":"family", "expand":true, "enable_counters":true,
            "group_domain":{"\"cool\"":[["state","=","alpha"]],"\"warm\"":[["state","=","beta"]]},
            "comodel_domain":[["id","in",[ids.cool_tag, ids.warm_tag]]],
        }),
    ));
    assert_eq!(
        filtered,
        json!({"values":[
            {"id":ids.cool_tag,"display_name":"Cool tag","group_id":"cool","group_name":"Cool","__count":2},
            {"id":ids.warm_tag,"display_name":"Warm tag","group_id":"warm","group_name":"Warm","__count":1},
        ]})
    );
    // Both the returned category list and the counter query are controlled by the caller domain.
    let exact = success(&fixture.session(
        RANGE,
        json!({
            "field_name":"category_id", "hierarchize":false, "expand":true, "enable_counters":true,
            "comodel_domain":[["id","=",ids.child]], "search_domain":[["state","=","alpha"]],
            "limit":2,
        }),
    ));
    assert_eq!(
        exact,
        json!({"parent_field":false,"values":[
            {"id":ids.child,"display_name":"Child","__count":1},
        ]})
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_reads_accept_browser_sessions_and_native_user_api_keys() {
    let fixture = PanelFixture::start().await;
    let token = fixture.key(fixture.user_id).await;
    // Native user keys deliberately inherit their subject without a restricted scope.
    // Do not manufacture a read-only key by bypassing the real store constraint.
    let refused = fixture
        .native
        .client
        .execute(
            "UPDATE res_users_apikeys SET read_only=true WHERE user_id=$1",
            &[&fixture.user_id],
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        Some(&erp_testkit::tokio_postgres::error::SqlState::CHECK_VIOLATION)
    );
    let db_error = refused.as_db_error().unwrap();
    assert_eq!(db_error.constraint(), Some("apikey_user"));
    assert_eq!(
        db_error.message(),
        "new row for relation \"res_users_apikeys\" violates check constraint \"apikey_user\""
    );
    for method in [RANGE, MULTI] {
        let body = json!({"field_name":"state", "expand":true, "enable_counters":true});
        let expected = selections(2, 1, method == RANGE);
        assert_eq!(
            success(&fixture.user_session(method, body.clone())),
            expected
        );
        assert_eq!(success(&fixture.bearer(method, body, &token)), expected);
    }
    fixture.cleanup().await;
}
