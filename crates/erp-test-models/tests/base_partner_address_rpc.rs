//! Real JSON-2 parent onchange acceptance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;
use erp_rpc::RpcConfig;
use erp_sdk::{Cell, RawVals};
use erp_testkit::RulesPolicy;
use serde_json::{Value, json};
use support::rpc::*;

#[tokio::test]
async fn parent_onchange_proposes_address_only_for_contact_without_persisting() {
    let h = harness_with_policy(
        &[&erp_addon_base::ADDON],
        RulesPolicy::new(FixturePolicy).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await;
    let mut setup = h.harness().begin().await.unwrap();
    let currencies = setup.env().records("res.currency").unwrap();
    let currency = currencies
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("TST".into()))
                    .set_cell("symbol", Cell::Str("T".into())),
            ],
        )
        .await
        .unwrap();
    let companies = setup.env().records("res.company").unwrap();
    let company = companies
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Address company".into()))
                    .set_cell("currency_id", Cell::Id(currency.ids()[0])),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    let response = call(
        &h,
        "/json/2/res.partner/create",
        &json!({"vals_list":[{"name":"Parent", "street":"Main", "street2":"Suite", "city":"City", "zip":"12345", "company_id":company}]}).to_string(),
        alice(),
    )
    .await;
    let parent = ok(&response)[0].as_i64().unwrap();
    let persistent_before = count(&h, "res_partner").await;
    let spec = json!({"name":{}, "type":{}, "parent_id":{"fields":{"display_name":{}}}, "street":{}, "street2":{}, "city":{}, "zip":{}, "company_id":{"fields":{"display_name":{}}}});
    for ty in ["contact", "invoice", "delivery", "other"] {
        let response = call(&h, "/json/2/res.partner/onchange", &json!({"values":{"name":"Unsaved", "type":ty, "parent_id":parent}, "field_names":["parent_id"], "fields_spec":spec}).to_string(), alice()).await;
        let result = ok(&response);
        assert_eq!(
            result["value"]["company_id"]["id"],
            json!(company),
            "{result}"
        );
        if ty == "contact" {
            assert_eq!(result["value"]["street"], json!("Main"));
            assert_eq!(result["value"]["city"], json!("City"));
            assert_eq!(result["value"]["street2"], json!("Suite"));
            assert_eq!(result["value"]["zip"], json!("12345"));
        } else {
            for field in ["street", "street2", "city", "zip"] {
                assert!(
                    result["value"].get(field).is_none()
                        || result["value"][field] == Value::Bool(false),
                    "{field}: {result}"
                );
            }
        }
    }
    let response = call(
        &h,
        "/json/2/res.partner/create",
        &json!({"vals_list":[{"name":"Empty parent"}]}).to_string(),
        alice(),
    )
    .await;
    let empty_parent = ok(&response)[0].as_i64().unwrap();
    // A new request with an empty-address parent must not reuse the prior proposal.
    let response = call(&h, "/json/2/res.partner/onchange", &json!({"values":{"name":"Unsaved", "type":"contact", "parent_id":empty_parent}, "field_names":["parent_id"], "fields_spec":spec}).to_string(), alice()).await;
    let result = ok(&response);
    for field in ["street", "street2", "city", "zip"] {
        assert!(
            result["value"].get(field).is_none() || result["value"][field] == Value::Bool(false),
            "{field}: {result}"
        );
    }
    // A second edit of the populated parent produces a fresh proposal.
    let response = call(&h, "/json/2/res.partner/onchange", &json!({"values":{"name":"Unsaved again", "type":"contact", "parent_id":parent}, "field_names":["parent_id"], "fields_spec":spec}).to_string(), alice()).await;
    assert_eq!(ok(&response)["value"]["street"], json!("Main"));
    assert_eq!(
        count(&h, "res_partner").await,
        persistent_before + 1,
        "virtual contacts must not persist"
    );
    let response = call(
        &h,
        "/json/2/res.partner/read",
        &json!({"ids":[parent], "fields":["name", "street"]}).to_string(),
        alice(),
    )
    .await;
    let read = ok(&response);
    assert_eq!(read[0]["name"], json!("Parent"));
    assert_eq!(read[0]["street"], json!("Main"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn address_helpers_are_not_rpc_endpoints() {
    let h = harness_with_policy(
        &[&erp_addon_base::ADDON],
        RulesPolicy::new(FixturePolicy).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await;
    for method in [
        "_address_fields",
        "_formatting_address_fields",
        "_fields_sync",
        "address_get",
    ] {
        let response = call(&h, &format!("/json/2/res.partner/{method}"), "{}", alice()).await;
        assert_error(
            &response,
            403,
            "erp.exceptions.AccessError",
            &format!("Private methods (such as 'res.partner.{method}') cannot be called remotely."),
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn parent_onchange_refuses_a_parent_outside_the_read_rule() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "res.partner",
            erp_sdk::Operation::Read,
            erp_sdk::Domain::condition("name", erp_sdk::Operator::Ne, "Hidden address parent")
                .unwrap(),
        )
        .build()
        .unwrap();
    let h = harness_with_policy(
        &[&erp_addon_base::ADDON],
        policy,
        RpcConfig::default(),
        false,
    )
    .await;
    let mut setup = h.harness().begin().await.unwrap();
    let partners = setup.env().records("res.partner").unwrap();
    let parent = partners
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Hidden address parent".into()))
                    .set_cell("street", Cell::Str("Private street".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    let count_before = count(&h, "res_partner").await;
    let response = call(
        &h,
        "/json/2/res.partner/onchange",
        &json!({
            "values":{"name":"Unsaved", "type":"contact", "parent_id":parent},
            "field_names":["parent_id"],
            "fields_spec":{"name":{}, "type":{}, "parent_id":{}, "street":{}}
        })
        .to_string(),
        alice(),
    )
    .await;
    assert_error(
        &response,
        403,
        "erp.exceptions.AccessError",
        &format!(
            "Your access rights do not let you access 'Contact' (res.partner) records.\n\nRecords: res.partner({parent},), User: 2"
        ),
    );
    assert_eq!(count(&h, "res_partner").await, count_before);
    h.cleanup().await.unwrap();
}
