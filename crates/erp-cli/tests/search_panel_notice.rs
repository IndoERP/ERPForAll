//! Extended panels retain visible view-selection and counter compatibility notices together.
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
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extended_panel_retains_view_fallback_notice() {
    let fixture = PanelFixture::start().await;
    let search = r#"<search><searchpanel><field name="state" enable_counters="1"/><field name="tag_ids" select="multi" domain="[('family', '=', state)]"/></searchpanel></search>"#;
    let created = success(&native_http::json_post(fixture.native.port, &fixture.native.cookies,
        "/erp/json/2/ir.ui.view/create", &json!({"vals_list":[
            {"name":"Notice search","model":ITEM,"type":"search","priority":0,"arch_db":search},
            {"name":"Notice list","model":ITEM,"type":"list","arch_db":"<list><field name='name'/></list>"}
        ]}).to_string()));
    let list = created[1].as_i64().unwrap();
    let response = fixture.user_session(
        "get_views",
        json!({"views":[[list,"list"]],"wire_major":1,
        "context":{"search_view_ref":"panel_missing.search"}}),
    );
    let body = success(&response);
    let panel = &body["views"][0]["nodes"][0]["search_panel"];
    assert_eq!(panel["kind"], "sections");
    assert_eq!(panel["sections"][0]["enable_counters"], false);
    assert_eq!(
        panel["notice"],
        "Category counters are unavailable while a filter domain is active. 'search_view_ref': no view 'panel_missing.search'; the default view is used"
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_panel_attributes_preserve_list_opening() {
    let f = PanelFixture::start().await;
    let created=success(&native_http::json_post(f.native.port,&f.native.cookies,
        "/erp/json/2/ir.ui.view/create",&json!({"vals_list":[
            {"name":"Syntax search","model":ITEM,"type":"search","priority":0,"arch_db":"<search/>"},
            {"name":"Syntax list","model":ITEM,"type":"list","arch_db":"<list><field name='name'/></list>"}
        ]}).to_string()));
    let search = created[0].as_i64().unwrap();
    let list = created[1].as_i64().unwrap();
    let cases = [
        (
            "<searchpanel class='private-marker'><field name='state'/></searchpanel>",
            0,
            "panel",
        ),
        (
            "<searchpanel><field name='state' icon_class='fa fa-private-marker'/></searchpanel>",
            1,
            "section",
        ),
        (
            "<searchpanel><field name='category_id' depth='2'/></searchpanel>",
            1,
            "section",
        ),
        (
            "<searchpanel class='private-marker' view_types='kanban'><field name='state'/></searchpanel>",
            0,
            "panel",
        ),
    ];
    for (inner, id, kind) in cases {
        let arch = format!("<search>{inner}</search>");
        f.native
            .client
            .execute(
                "UPDATE ir_ui_view SET arch_db=$1 WHERE id=$2",
                &[&arch, &search],
            )
            .await
            .unwrap();
        success(&native_http::json_post(
            f.native.port,
            &f.native.cookies,
            "/erp/json/2/ir.ui.view/write",
            &json!({"ids":[search],"vals":{"name":format!("Syntax {kind} {id}")}}).to_string(),
        ));
        let message = format!(
            "Search panel section {id} has unsupported {kind} syntax; additional syntax requires a later release."
        );
        let standalone = f.user_session(
            "get_views",
            json!({"views":[[search,"search"]],"wire_major":1}),
        );
        assert_eq!(standalone.status, 422, "{}", standalone.body);
        let error: serde_json::Value = serde_json::from_str(&standalone.body).unwrap();
        assert_eq!(error["name"], "erp.exceptions.ValidationError");
        assert_eq!(error["message"], message);
        assert!(!standalone.body.contains("private-marker"));
        if inner.contains("depth=") {
            assert!(!error["message"].as_str().unwrap().contains('2'));
        }
        let body =
            success(&f.user_session("get_views", json!({"views":[[list,"list"]],"wire_major":1})));
        let root = &body["views"][0]["nodes"][0];
        assert_eq!(root["kind"], "list");
        assert_eq!(
            root["search_panel"],
            json!({"kind":"unsupported","message":message})
        );
        assert!(!body.to_string().contains("private-marker"));
        // A successful ordinary data read proves that unsupported panel metadata adds no predicate.
        let rows = success(&f.user_session(
            "search_read",
            json!({"domain":[],"fields":["name"],"limit":10}),
        ));
        assert!(!rows.as_array().unwrap().is_empty());
    }
    f.cleanup().await;
}
