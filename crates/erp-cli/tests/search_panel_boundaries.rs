//! Exact native admission boundaries; failures must publish an error, never partial metadata.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "native fixture assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/search_panel_fixture.rs"]
mod search_panel_fixture;
use search_panel_fixture::{ITEM, PanelFixture, success};
use serde_json::{Value, json};
async fn describe(fixture: &PanelFixture, arch: &str) -> native_http::Reply {
    let ids = success(&native_http::json_post(fixture.native.port, &fixture.native.cookies,
        "/erp/json/2/ir.ui.view/create", &json!({"vals_list":[{"name":"Panel admission boundary","model":ITEM,"type":"search","arch_db":arch}]}).to_string()));
    fixture.user_session(
        "get_views",
        json!({"views":[[ids[0],"search"]],"wire_major":1}),
    )
}
fn panel(body: &Value) -> &Value {
    &body["views"][0]["nodes"][0]["search_panel"]
}
fn refusal(reply: native_http::Reply, message: &str) {
    assert_eq!(reply.status, 422, "{}", reply.body);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["name"], "erp.exceptions.ValidationError");
    assert_eq!(body["message"], message);
    assert!(body.get("views").is_none());
    assert!(body.get("fields").is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_exact_boundaries_publish_atomically() {
    let fixture = PanelFixture::start().await;
    let source = |count| {
        format!(
            "<search><searchpanel>{}</searchpanel></search>",
            "<field name='state' select='multi'/>".repeat(count)
        )
    };
    let body = success(&describe(&fixture, &source(64)).await);
    assert_eq!(panel(&body)["sections"].as_array().unwrap().len(), 64);
    refusal(
        describe(&fixture, &source(65)).await,
        "Search panel section 65 exceeds section count limit.",
    );
    let source = |icon: &str, label: &str| {
        format!(
            "<search><searchpanel><field name='state' icon='{icon}' string='{label}'/></searchpanel></search>"
        )
    };
    let body = success(&describe(&fixture, &source(&"a".repeat(64), &"é".repeat(128))).await);
    assert_eq!(panel(&body)["sections"][0]["icon"], "a".repeat(64));
    assert_eq!(panel(&body)["sections"][0]["label"], "é".repeat(128));
    refusal(
        describe(&fixture, &source(&"a".repeat(65), "Label")).await,
        "Search panel section 1 exceeds icon byte limit.",
    );
    refusal(
        describe(
            &fixture,
            &source("folder", &format!("{}a", "é".repeat(128))),
        )
        .await,
        "Search panel section 1 exceeds label byte limit.",
    );
    let source = |types: &str| {
        format!(
            "<search><searchpanel view_types='{types}'><field name='state' icon='folder'/></searchpanel></search>"
        )
    };
    let keys = std::iter::once("list".to_owned())
        .chain((1..64).map(|i| format!("a{i}")))
        .collect::<Vec<_>>();
    let body = success(&describe(&fixture, &source(&keys.join(","))).await);
    assert_eq!(panel(&body)["view_types"].as_array().unwrap().len(), 64);
    refusal(
        describe(&fixture, &source(&format!("{},extra", keys.join(",")))).await,
        "Search panel section 0 exceeds view key count limit.",
    );
    let qualified = format!("{}.{}", "a".repeat(64), "b".repeat(64));
    let body = success(&describe(&fixture, &source(&qualified)).await);
    assert_eq!(panel(&body)["view_types"], json!([qualified]));
    refusal(
        describe(
            &fixture,
            &source(&format!("{}.{}", "a".repeat(65), "b".repeat(64))),
        )
        .await,
        "Search panel section 0 exceeds view key byte limit.",
    );
    for limit in [0u64, 9_007_199_254_740_991] {
        let body=success(&describe(&fixture,&format!("<search><searchpanel><field name='state' icon='folder' limit='{limit}'/></searchpanel></search>")).await);
        assert_eq!(panel(&body)["sections"][0]["limit"], limit);
    }
    refusal(describe(&fixture,"<search><searchpanel><field name='state' icon='folder' limit='9007199254740992'/></searchpanel></search>").await,"Search panel section 1 exceeds display limit.");
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_panel_selection_default_key_byte_limit_is_located() {
    let exact = "é".repeat(128);
    let fixture = PanelFixture::start_with(|addons| {
        let file = addons.join("panel_http/models/panel.toml");
        let mut source = std::fs::read_to_string(&file).unwrap();
        // The default fixture ends with panel_http.item; only this opt-in instance gets a field.
        source.push_str(&format!("\n[[model.field]]\nname='wide_state'\ntype='selection'\nselection=[['{exact}','Wide key']]\n"));
        std::fs::write(file, source).unwrap();
    }).await;
    let ids = success(&native_http::json_post(fixture.native.port, &fixture.native.cookies,
        "/erp/json/2/ir.ui.view/create", &json!({"vals_list":[{"name":"Panel default key boundary","model":ITEM,"type":"search","arch_db":"<search><searchpanel><field name='wide_state' icon='folder'/></searchpanel></search>"}]}).to_string()));
    let request = |key: &str| {
        fixture.user_session("get_views", json!({"views":[[ids[0],"search"]],"wire_major":1,"context":{"searchpanel_default_wide_state":key}}))
    };
    let body = success(&request(&exact));
    assert_eq!(panel(&body)["sections"][0]["id"], 1);
    assert_eq!(
        panel(&body)["sections"][0]["default_values"],
        json!([exact])
    );
    let over = format!("{exact}x");
    let reply = request(&over);
    assert!(
        !reply.body.contains(&over),
        "oversized key must not appear in a response"
    );
    refusal(
        reply,
        "Search panel section 1 exceeds selection key byte limit.",
    );
    fixture.cleanup().await;
}
