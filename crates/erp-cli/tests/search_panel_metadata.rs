//! Final section authorization and caller overlays through native get_views.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/search_panel_fixture.rs"]
mod search_panel_fixture;
use native_http::{Reply, json_post};
use search_panel_fixture::{ITEM, PanelFixture};
use serde_json::{Value, json};
fn success(reply: Reply) -> Value {
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}
fn failure(reply: Reply, message: &str) {
    assert_eq!(reply.status, 422, "{}", reply.body);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["name"], "erp.exceptions.ValidationError");
    assert_eq!(body["message"], message);
}
fn views(f: &PanelFixture, view: i64, context: Value) -> Reply {
    f.user_session(
        "get_views",
        json!({"views":[[view,"search"]],"wire_major":1,"context":context}),
    )
}
async fn search(f: &PanelFixture, arch: &str) -> i64 {
    let values=success(json_post(f.native.port,&f.native.cookies,"/erp/json/2/ir.ui.view/create",&json!({"vals_list":[{"name":"Panel sections","model":ITEM,"type":"search","arch_db":arch}]}).to_string()));
    values[0].as_i64().unwrap()
}
/// Exercise final H18 admission of a fresh stored arch without changing the ordinary XML
/// write validator's restricted-expression rule. No request has compiled this fresh view yet.
async fn raw_search(f: &PanelFixture, arch: &str) -> i64 {
    let id = search(f, "<search/>").await;
    f.native
        .client
        .execute(
            "UPDATE ir_ui_view SET arch_db=$1 WHERE id=$2",
            &[&arch, &id],
        )
        .await
        .unwrap();
    // Name-only ORM writes invalidate the real templates cache without asking the ordinary
    // XML validator to admit this intentionally adversarial stored architecture.
    success(json_post(
        f.native.port,
        &f.native.cookies,
        "/erp/json/2/ir.ui.view/write",
        &json!({"ids":[id],"vals":{"name":"Stored negative panel fixture"}}).to_string(),
    ));
    id
}
fn panel(body: &Value) -> &Value {
    &body["views"][0]["nodes"][0]["search_panel"]
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sections_prune_dependencies_and_overlay_only_retained_defaults() {
    let f = PanelFixture::start().await;
    let id = raw_search(
        &f,
        r#"<search><searchpanel>
      <field name="secret_state" enable_counters="1"/>
      <field name="state" enable_counters="1"/>
      <field name="tag_ids" select="multi" domain="[('family', '=', secret_state)]"/>
      <field name="category_id" select="multi"/>
    </searchpanel></search>"#,
    )
    .await;
    let body = success(views(
        &f,
        id,
        json!({"searchpanel_default_state":"alpha","searchpanel_default_secret_state":{"hidden":"must not echo"},"searchpanel_default_category_id":[f.ids.root]}),
    ));
    let sections = panel(&body)["sections"].as_array().unwrap();
    assert_eq!(
        sections
            .iter()
            .map(|s| s["field"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["state", "category_id"]
    );
    assert_eq!(sections[0]["id"], 2);
    assert_eq!(sections[1]["id"], 4);
    assert_eq!(sections[0]["default_values"], json!(["alpha"]));
    assert_eq!(sections[1]["default_values"], json!([f.ids.root]));
    assert_eq!(sections[0]["enable_counters"], true);
    assert!(panel(&body)["notice"].is_null());
    assert!(body["fields"][ITEM]["secret_state"].is_null());
    let other = success(views(
        &f,
        id,
        json!({"searchpanel_default_state":"beta","searchpanel_default_category_id":[f.ids.child]}),
    ));
    assert_eq!(
        panel(&other)["sections"][0]["default_values"],
        json!(["beta"])
    );
    assert_eq!(
        panel(&success(views(&f, id, json!({}))))["sections"][0]["default_values"],
        json!([])
    );
    for (value, message) in [
        (
            json!(["alpha"]),
            "Search panel section 2 has an invalid category default.",
        ),
        (
            json!("unknown"),
            "Search panel section 2 has an invalid selection default.",
        ),
    ] {
        failure(
            views(&f, id, json!({"searchpanel_default_state":value})),
            message,
        );
    }
    let peer = f.native.peer();
    let peer_body=success(json_post(peer.port,&f.user_cookies,&format!("/erp/json/2/{ITEM}/get_views"),&json!({"views":[[id,"search"]],"wire_major":1,"context":{"searchpanel_default_state":"alpha","searchpanel_default_category_id":[f.ids.root]}}).to_string()));
    assert_eq!(panel(&peer_body), panel(&body));
    drop(peer);
    let domain_id=search(&f,r#"<search><searchpanel><field name="state" enable_counters="1"/><field name="tag_ids" select="multi" domain="[('family', '=', state)]"/></searchpanel></search>"#).await;
    let counters = success(views(&f, domain_id, json!({})));
    assert_eq!(panel(&counters)["sections"][0]["enable_counters"], false);
    assert_eq!(
        panel(&counters)["notice"],
        "Category counters are unavailable while a filter domain is active."
    );
    for arch in [
        r#"<search><searchpanel><field name="state" enable_counters="1"/><field name="tag_ids" select="multi" domain="[('secret_family', '=', state)]"/></searchpanel></search>"#,
        r#"<search><searchpanel><field name="state" enable_counters="1"/><field name="tag_ids" select="multi" groupby="secret_family"/></searchpanel></search>"#,
    ] {
        let denied = raw_search(&f, arch).await;
        let body = success(views(&f, denied, json!({})));
        assert_eq!(panel(&body)["kind"], "sections", "{body}");
        assert_eq!(panel(&body)["sections"].as_array().unwrap().len(), 1);
        assert_eq!(panel(&body)["sections"][0]["field"]["name"], "state");
        assert_eq!(panel(&body)["sections"][0]["enable_counters"], true);
        assert!(panel(&body)["notice"].is_null());
    }
    for (value, message) in [
        (
            json!([0]),
            "Search panel section 4 has an invalid relational default.",
        ),
        (
            json!([f.ids.root, f.ids.root]),
            "Search panel section 4 has duplicate defaults.",
        ),
        (
            json!([9007199254740992u64]),
            "Search panel section 4 has an invalid relational default.",
        ),
        (
            json!(f.ids.root),
            "Search panel section 4 requires an array default.",
        ),
        (
            json!((1..=65).collect::<Vec<i64>>()),
            "Search panel section 4 exceeds default count limit.",
        ),
    ] {
        failure(
            views(&f, id, json!({"searchpanel_default_category_id":value})),
            message,
        );
    }
    let max_defaults = success(views(
        &f,
        id,
        json!({"searchpanel_default_category_id":(1..=64).collect::<Vec<i64>>()}),
    ));
    assert_eq!(
        panel(&max_defaults)["sections"][1]["default_values"]
            .as_array()
            .unwrap()
            .len(),
        64
    );
    let legacy = search(
        &f,
        "<search><searchpanel><field name='state'/></searchpanel></search>",
    )
    .await;
    for clear in [json!(false), json!(0), json!("")] {
        let body = success(views(
            &f,
            legacy,
            json!({"searchpanel_default_state":clear}),
        ));
        assert_eq!(panel(&body)["kind"], "selection");
        assert!(panel(&body)["default_value"].is_null());
    }
    let categories=search(&f,r#"<search><searchpanel><field name="state"/><field name="category_id"/></searchpanel></search>"#).await;
    let cleared = success(views(
        &f,
        categories,
        json!({"searchpanel_default_category_id":false}),
    ));
    assert_eq!(panel(&cleared)["sections"][1]["default_values"], json!([]));
    for invalid in [json!(0), json!("")] {
        failure(
            views(
                &f,
                categories,
                json!({"searchpanel_default_category_id":invalid}),
            ),
            "Search panel section 2 has an invalid relational default.",
        );
    }
    let admin = success(f.session(
        "get_views",
        json!({"views":[[categories,"search"]],"wire_major":1,"context":{"searchpanel_default_state":"beta","searchpanel_default_category_id":f.ids.root}}),
    ));
    assert_eq!(
        panel(&admin)["sections"][0]["default_values"],
        json!(["beta"])
    );
    let user = success(views(
        &f,
        categories,
        json!({"searchpanel_default_state":"alpha"}),
    ));
    assert_eq!(
        panel(&user)["sections"][0]["default_values"],
        json!(["alpha"])
    );
    assert_eq!(panel(&user)["sections"][1]["default_values"], json!([]));
    let chosen = success(views(
        &f,
        categories,
        json!({"searchpanel_default_category_id":f.ids.child}),
    ));
    assert_eq!(
        panel(&chosen)["sections"][1]["default_values"],
        json!([f.ids.child])
    );
    let kanban=success(json_post(f.native.port,&f.native.cookies,"/erp/json/2/ir.ui.view/create",&json!({"vals_list":[{"name":"Kanban panel","model":ITEM,"type":"search","priority":0,"arch_db":"<search><searchpanel view_types='kanban'><field name='state'/><field name='tag_ids' select='multi'/></searchpanel></search>"}]}).to_string()))[0].as_i64().unwrap();
    let standalone = success(views(&f, kanban, json!({})));
    assert_eq!(panel(&standalone)["view_types"], json!(["kanban"]));
    let list=success(json_post(f.native.port,&f.native.cookies,"/erp/json/2/ir.ui.view/create",&json!({"vals_list":[{"name":"List without panel fields","model":ITEM,"type":"list","arch_db":"<list><field name='name'/></list>"}]}).to_string()))[0].as_i64().unwrap();
    let body =
        success(f.user_session("get_views", json!({"views":[[list,"list"]],"wire_major":1})));
    assert!(panel(&body).is_null());
    assert!(body["fields"][ITEM]["state"].is_null());
    assert!(body["fields"][ITEM]["tag_ids"].is_null());
    let malformed=search(&f,r#"<search><searchpanel><field name="secret_state" icon="private-value://hidden"/><field name="state"/></searchpanel></search>"#).await;
    failure(
        views(&f, malformed, json!({})),
        "Search panel section 1 has an invalid icon.",
    );
    let key = format!("{}.{}", "a".repeat(64), "b".repeat(64));
    let qualified = search(
        &f,
        &format!(
            "<search><searchpanel view_types='{key}'><field name='state'/></searchpanel></search>"
        ),
    )
    .await;
    let body = success(views(&f, qualified, json!({})));
    assert_eq!(panel(&body)["view_types"], json!([key]));
    let grouped=search(&f,r#"<search><searchpanel><field name="state" string="Private XML label" groups="base.group_system"/><field name="tag_ids" select="multi" domain="[('family','=',state)]"/><field name="category_id"/></searchpanel></search>"#).await;
    let body = success(views(&f, grouped, json!({})));
    assert_eq!(panel(&body)["sections"].as_array().unwrap().len(), 1);
    assert_eq!(panel(&body)["sections"][0]["id"], 3);
    assert!(!body.to_string().contains("Private XML label"));
    let hidden=search(&f,r#"<search><searchpanel groups="base.group_system"><field name="state"/><field name="tag_ids" select="multi"/></searchpanel></search>"#).await;
    assert!(panel(&success(views(&f, hidden, json!({})))).is_null());
    let unknown = raw_search(&f, "<search><searchpanel><field name='private_panel_marker'/><field name='state'/></searchpanel></search>").await;
    let error = views(&f, unknown, json!({}));
    assert!(!error.body.contains("private_panel_marker"));
    failure(
        error,
        "Search panel section 1 references an invalid root field.",
    );
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_whitespace_domain_is_refused_at_admission() {
    let f = PanelFixture::start().await;
    let id=raw_search(&f,"<search><searchpanel><field name='state'/><field name='tag_ids' select='multi' domain='  '/></searchpanel></search>").await;
    failure(
        views(&f, id, json!({})),
        "Search panel section 2 has an invalid domain.",
    );
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_plain_relational_sections_require_comodel_read() {
    let f = PanelFixture::start_with(|addons| {
        let path = addons.join("panel_http/security/ir.access.csv");
        let access = std::fs::read_to_string(&path).unwrap();
        let access = access
            .lines()
            .filter(|line| !line.starts_with("category_user,") && !line.starts_with("tag_user,"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, access).unwrap();
    })
    .await;
    for field in [
        "<field name='category_id' string='Private relation label'/>",
        "<field name='tag_ids' select='multi' string='Private relation label'/>",
    ] {
        let id = raw_search(
            &f,
            &format!("<search><searchpanel><field name='state'/>{field}</searchpanel></search>"),
        )
        .await;
        let body = success(views(&f, id, json!({})));
        assert_eq!(panel(&body)["kind"], "sections");
        assert_eq!(panel(&body)["sections"].as_array().unwrap().len(), 1);
        assert_eq!(panel(&body)["sections"][0]["id"], 1);
        assert_eq!(panel(&body)["sections"][0]["field"]["name"], "state");
        assert!(!body.to_string().contains("Private relation label"));
        // independently publishes readable root fields for the search editor.
        // Pruning the section must not publish a denied comodel field catalogue.
        assert!(body["fields"][search_panel_fixture::CATEGORY].is_null());
        assert!(body["fields"][search_panel_fixture::TAG].is_null());
    }
    f.cleanup().await;
}
