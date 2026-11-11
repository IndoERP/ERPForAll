//! Authenticated JSON-2 New-form requests retain commercial ancestry only in the cache.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_rpc::RpcConfig;
use erp_testkit::{RpcHarness, RulesPolicy};
use serde_json::{Value, json};
use support::rpc::*;

async fn harness() -> RpcHarness {
    harness_with_policy(
        &[&erp_addon_base::ADDON],
        RulesPolicy::new(FixturePolicy).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await
}

fn fields_spec(expanded: bool) -> Value {
    let commercial = if expanded {
        json!({"fields":{"display_name":{}}})
    } else {
        json!({})
    };
    json!({
        "name":{}, "type":{}, "complete_name":{}, "display_name":{},
        "commercial_partner_id":commercial,
        "commercial_company_name":{},
        "child_ids":{"fields":{
            "name":{}, "type":{}, "complete_name":{}, "display_name":{},
            "commercial_partner_id":commercial,
            "commercial_company_name":{}
        }}
    })
}

fn assert_new_commercial_value(value: &Value, expanded: bool) {
    // web_read expands an originless target to False; without a sub-spec it emits JSON null.
    // Neither representation leaks the internal negative RecId as a persistent identity.
    assert_eq!(value, &if expanded { json!(false) } else { Value::Null });
}

#[tokio::test]
async fn new_root_and_child_first_onchange_resolve_defaults_without_persisting() {
    let h = harness().await;
    let before = count(&h, "res_partner").await;
    for expanded in [false, true] {
        for name in ["First draft", "Second draft"] {
            let context = json!({"default_name":name, "default_type":"contact"});
            let response = call(
                &h,
                "/json/2/res.partner/default_get",
                &json!({"fields":["name", "type"], "context":context}).to_string(),
                alice(),
            )
            .await;
            assert_eq!(ok(&response), json!({"name":name, "type":"contact"}));
            let response = call(
                &h,
                "/json/2/res.partner/onchange",
                &json!({
                    "values":{"child_ids":[[0,"virtual_child",{"name":"Draft child", "type":"contact"}]]},
                    "field_names":[], "fields_spec":fields_spec(expanded), "context":context
                }).to_string(),
                alice(),
            ).await;
            let result = ok(&response);
            let values = &result["value"];
            assert_eq!(values["name"], json!(name));
            assert_eq!(values["type"], json!("contact"));
            assert_eq!(values["complete_name"], json!(name));
            assert_eq!(values["display_name"], json!(name));
            assert_eq!(values["commercial_company_name"], json!(name));
            assert_new_commercial_value(&values["commercial_partner_id"], expanded);
            let commands = values["child_ids"].as_array().expect("child command list");
            assert_eq!(commands.len(), 1, "{result}");
            assert_eq!(commands[0][0], json!(0));
            assert_eq!(commands[0][1], json!("virtual_child"));
            let child = &commands[0][2];
            assert_eq!(child["name"], json!("Draft child"));
            assert_eq!(
                child["complete_name"],
                json!(format!("{name}, Draft child"))
            );
            assert_eq!(child["display_name"], json!(format!("{name}, Draft child")));
            assert_eq!(child["commercial_company_name"], json!(name));
            assert_new_commercial_value(&child["commercial_partner_id"], expanded);
            assert_eq!(count(&h, "res_partner").await, before);
        }
    }
    // A later field-change request also starts from an isolated unsaved record.
    let response = call(
        &h,
        "/json/2/res.partner/onchange",
        &json!({
            "values":{"name":"Changed draft", "type":"contact"},
            "field_names":["name"], "fields_spec":fields_spec(true)
        })
        .to_string(),
        alice(),
    )
    .await;
    let result = ok(&response);
    assert_eq!(result["value"]["complete_name"], json!("Changed draft"));
    assert_eq!(result["value"]["display_name"], json!("Changed draft"));
    assert_eq!(
        result["value"]["commercial_company_name"],
        json!("Changed draft")
    );
    assert_eq!(count(&h, "res_partner").await, before);

    let response = call(
        &h,
        "/json/2/res.partner/create",
        &json!({"vals_list":[{"name":"Saved after onchange"}]}).to_string(),
        alice(),
    )
    .await;
    let id = ok(&response)[0].as_i64().expect("persisted positive id");
    assert!(id > 0);
    let response = call(
        &h,
        "/json/2/res.partner/read",
        &json!({"ids":[id], "fields":["name", "commercial_partner_id", "commercial_company_name"], "load":""}).to_string(),
        alice(),
    ).await;
    let saved = ok(&response);
    assert_eq!(saved[0]["name"], json!("Saved after onchange"));
    assert_eq!(saved[0]["commercial_partner_id"], json!(id));
    assert_eq!(
        saved[0]["commercial_company_name"],
        json!("Saved after onchange")
    );
    assert_eq!(count(&h, "res_partner").await, before + 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_partner_onchange_keeps_create_permission_checks() {
    let h = harness().await;
    let before = count(&h, "res_partner").await;
    let response = call(
        &h,
        "/json/2/res.partner/onchange",
        &json!({"values":{"name":"Denied draft"}, "field_names":[], "fields_spec":fields_spec(true)}).to_string(),
        user(BOB),
    ).await;
    assert_error(
        &response,
        403,
        "erp.exceptions.AccessError",
        "You are not allowed to create 'Contact' (res.partner) records.",
    );
    assert_eq!(count(&h, "res_partner").await, before);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_partner_onchange_refuses_internal_negative_ids_from_the_client() {
    let h = harness().await;
    let before = count(&h, "res_partner").await;
    let forged = -(1_i64 << 62) - 1;
    let response = call(
        &h,
        "/json/2/res.partner/onchange",
        &json!({
            "values":{"name":"Forged draft", "parent_id":forged},
            "field_names":["parent_id"],
            "fields_spec":{"name":{}, "parent_id":{}}
        })
        .to_string(),
        alice(),
    )
    .await;
    assert_error(
        &response,
        422,
        "erp.exceptions.ValidationError",
        &format!("Invalid value for res.partner.parent_id: {forged}"),
    );
    assert_eq!(count(&h, "res_partner").await, before);
    h.cleanup().await.unwrap();
}
