//! Real JSON-2 dispatch acceptance for contact labels and lookup.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_addon_base::ResPartner;
use erp_orm::{Operation, PrincipalBinding};
use erp_registry::ModelSpec;
use erp_rpc::RpcConfig;
use erp_sdk::{
    Cell, Domain, ErrorKind, NameSearch, Operator, RawVals, Result, Transaction,
    Value as FieldValue,
};
use erp_testkit::{RpcHarness, RulesPolicy, TestRules};
use serde_json::{Value, json};
use support::rpc::*;

#[derive(Clone, Copy)]
struct PartnerRules;

impl TestRules for PartnerRules {
    fn check_model(
        &self,
        binding: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        FixturePolicy.check_model(binding, model, op)
    }

    fn access_domain(
        &self,
        binding: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        let existing = FixturePolicy.access_domain(binding, model, op)?;
        if model.name == "res.partner" && op == Operation::Read {
            Ok(existing & Domain::condition("name", Operator::Ne, "Hidden contact")?)
        } else {
            Ok(existing)
        }
    }
}

async fn harness() -> RpcHarness {
    harness_with_policy(
        &[&erp_addon_base::ADDON],
        RulesPolicy::new(PartnerRules).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await
}

async fn create(h: &RpcHarness, values: Value) -> i64 {
    let response = call(
        h,
        "/json/2/res.partner/create",
        &json!({"vals_list": [values]}).to_string(),
        alice(),
    )
    .await;
    ok(&response)[0].as_i64().unwrap()
}

async fn name_create(h: &RpcHarness, name: &str, context: Value) -> Value {
    let response = call(
        h,
        "/json/2/res.partner/name_create",
        &json!({"name": name, "context": context}).to_string(),
        alice(),
    )
    .await;
    ok(&response)
}

async fn lookup(h: &RpcHarness, body: Value) -> Value {
    let response = call(
        h,
        "/json/2/res.partner/name_search",
        &body.to_string(),
        alice(),
    )
    .await;
    ok(&response)
}

fn typed_pairs(pairs: Vec<(i64, Option<Arc<str>>)>) -> Value {
    Value::Array(
        pairs
            .into_iter()
            .map(|(id, label)| json!([id, label.as_deref()]))
            .collect(),
    )
}

#[tokio::test]
async fn spec_partner_037_rpc_four_field_matches_preserve_domains_and_record_rules() {
    let h = harness().await;
    let acme = create(&h, json!({"name": "Acme"})).await;
    let john = create(&h, json!({"name": "John", "parent_id": acme})).await;
    let email = create(&h, json!({"name": "Email target", "email": "raoul@x.fr"})).await;
    let reference = create(&h, json!({"name": "Reference target", "ref": "R-77"})).await;
    let vat = create(&h, json!({"name": "VAT target", "vat": "BE0477472701"})).await;
    create(&h, json!({"name": "Unrelated"})).await;
    let hidden = {
        let mut tx = h.harness().begin().await.unwrap();
        let hidden = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Hidden contact".into()))
                        .set_cell("email", Cell::Str("raoul@hidden.example".into()))
                        .set_cell("ref", Cell::Str("R-77-hidden".into()))
                        .set_cell("vat", Cell::Str("BE0477HIDDEN".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        hidden
    };
    let mut tx = h.harness().begin_as(ALICE).await.unwrap();
    let partners = tx.env().model::<ResPartner>().unwrap();
    for (name, expected) in [
        ("John", json!([[john, "Acme, John"]])),
        ("raoul@", json!([[email, "Email target"]])),
        ("R-77", json!([[reference, "Reference target"]])),
        ("0477", json!([[vat, "VAT target"]])),
        ("acme", json!([[acme, "Acme"], [john, "Acme, John"]])),
    ] {
        assert_eq!(lookup(&h, json!({"name": name})).await, expected);
        assert_eq!(
            typed_pairs(partners.name_search(&mut tx, name).await.unwrap()),
            expected
        );
    }
    assert_eq!(
        lookup(&h, json!({"name": "raoul@", "domain": [["id", "=", john]]})).await,
        json!([])
    );
    for id in [john, hidden] {
        assert_eq!(
            typed_pairs(
                partners
                    .name_search_with(
                        &mut tx,
                        "raoul@",
                        &NameSearch::new()
                            .domain(Domain::condition("id", Operator::Eq, id).unwrap()),
                    )
                    .await
                    .unwrap()
            ),
            json!([])
        );
    }
    assert_eq!(
        lookup(
            &h,
            json!({"name": "raoul@", "domain": [["id", "=", hidden]]})
        )
        .await,
        json!([])
    );
    assert_eq!(
        typed_pairs(
            partners
                .name_search_with(
                    &mut tx,
                    "John",
                    &NameSearch::new().operator(Operator::EqILike),
                )
                .await
                .unwrap()
        ),
        json!([])
    );
    assert_eq!(
        lookup(&h, json!({"name": "John", "operator": "=ilike"})).await,
        json!([])
    );
    assert_eq!(
        lookup(
            &h,
            json!({"name": "Acme, John", "context": {"partner_display_name_hide_company": true}})
        )
        .await,
        json!([[john, "John"]])
    );
    assert_eq!(
        typed_pairs(
            partners
                .with_context([("partner_display_name_hide_company", FieldValue::Bool(true))])
                .name_search(&mut tx, "Acme, John")
                .await
                .unwrap()
        ),
        json!([[john, "John"]])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_038_rpc_name_search_respects_order_limits_and_archived() {
    let h = harness().await;
    let archived = name_create(&h, "\"A Raoul Grosbedon\" <raoul@example.fr>", json!({})).await[0]
        .as_i64()
        .unwrap();
    let archive = call(
        &h,
        "/json/2/res.partner/write",
        &json!({"ids": [archived], "vals": {"active": false}}).to_string(),
        alice(),
    )
    .await;
    ok(&archive);
    let b = name_create(&h, "B Raoul chirurgiens-dentistes.fr", json!({})).await[0]
        .as_i64()
        .unwrap();
    let c = name_create(
        &h,
        "C Raoul O'hara  <!@historicalsociety.museum>",
        json!({}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    let lone = name_create(&h, "ryu+giga-Sushi@example.jp", json!({})).await[0]
        .as_i64()
        .unwrap();
    let mut tx = h.harness().begin_as(ALICE).await.unwrap();
    let partners = tx.env().model::<ResPartner>().unwrap();
    assert_eq!(
        lookup(&h, json!({"name": "Raoul"})).await,
        json!([
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"]
        ])
    );
    assert_eq!(
        typed_pairs(partners.name_search(&mut tx, "Raoul").await.unwrap()),
        json!([
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"]
        ])
    );
    assert_eq!(
        lookup(&h, json!({"name": "Raoul", "limit": 1})).await,
        json!([[b, "B Raoul chirurgiens-dentistes.fr"]])
    );
    assert_eq!(
        typed_pairs(
            partners
                .name_search_with(&mut tx, "Raoul", &NameSearch::new().limit(Some(1)))
                .await
                .unwrap()
        ),
        json!([[b, "B Raoul chirurgiens-dentistes.fr"]])
    );
    assert_eq!(
        lookup(&h, json!({"name": ""})).await,
        json!([
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"],
            [lone, "ryu+giga-sushi@example.jp"]
        ])
    );
    assert_eq!(
        typed_pairs(partners.name_search(&mut tx, "").await.unwrap()),
        json!([
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"],
            [lone, "ryu+giga-sushi@example.jp"]
        ])
    );
    assert_eq!(
        lookup(&h, json!({"name": "", "limit": 1})).await,
        json!([[b, "B Raoul chirurgiens-dentistes.fr"]])
    );
    assert_eq!(
        typed_pairs(
            partners
                .name_search_with(&mut tx, "", &NameSearch::new().limit(Some(1)))
                .await
                .unwrap()
        ),
        json!([[b, "B Raoul chirurgiens-dentistes.fr"]])
    );
    assert_eq!(
        lookup(
            &h,
            json!({"name": "Raoul", "context": {"active_test": false}})
        )
        .await,
        json!([
            [archived, "A Raoul Grosbedon"],
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"]
        ])
    );
    assert_eq!(
        typed_pairs(
            partners
                .with_context([("active_test", FieldValue::Bool(false))])
                .name_search(&mut tx, "Raoul")
                .await
                .unwrap()
        ),
        json!([
            [archived, "A Raoul Grosbedon"],
            [b, "B Raoul chirurgiens-dentistes.fr"],
            [c, "C Raoul O'hara"]
        ])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_040_rpc_name_create_parsing_defaults_errors_and_readback() {
    let h = harness().await;
    let parent = create(&h, json!({"name": "Parent"})).await;
    let mut tx = h.harness().begin_as(ALICE).await.unwrap();
    let partners = tx.env().model::<ResPartner>().unwrap();
    for (input, name, email) in [
        (
            "\"Raoul Grosbedon\" <raoul@chirurgiens-dentistes.fr> ",
            "Raoul Grosbedon",
            json!("raoul@chirurgiens-dentistes.fr"),
        ),
        (
            "ryu+giga-Sushi@aizubange.fukushima.jp",
            "ryu+giga-sushi@aizubange.fukushima.jp",
            json!("ryu+giga-sushi@aizubange.fukushima.jp"),
        ),
        (
            "Raoul chirurgiens-dentistes.fr",
            "Raoul chirurgiens-dentistes.fr",
            json!(false),
        ),
        (
            " Raoul O'hara  <!@historicalsociety.museum>",
            "Raoul O'hara",
            json!("!@historicalsociety.museum"),
        ),
        (
            "Raoul Grosbedon <raoul@CHIRURGIENS-dentistes.fr> ",
            "Raoul Grosbedon",
            json!("raoul@chirurgiens-dentistes.fr"),
        ),
        (
            "Raoul megaraoul@chirurgiens-dentistes.fr",
            "Raoul",
            json!("megaraoul@chirurgiens-dentistes.fr"),
        ),
    ] {
        let result = name_create(&h, input, json!({"default_parent_id": parent})).await;
        let id = result[0].as_i64().unwrap();
        assert_eq!(result[1], format!("Parent, {name}"));
        let read = call(
            &h,
            "/json/2/res.partner/read",
            &json!({"ids": [id], "fields": ["name", "email", "display_name"]}).to_string(),
            alice(),
        )
        .await;
        assert_eq!(
            ok(&read),
            json!([{"id": id, "name": name, "email": email, "display_name": result[1]}])
        );
        let (typed_id, typed_label) = partners
            .with_context([("default_parent_id", FieldValue::Int(parent))])
            .name_create(&mut tx, input)
            .await
            .unwrap();
        let typed_partner = partners.browse([typed_id]).unwrap();
        assert_eq!(typed_label.as_ref(), format!("Parent, {name}"));
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::NAME)
                .await
                .unwrap()
                .as_deref(),
            Some(name)
        );
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::EMAIL)
                .await
                .unwrap()
                .as_deref(),
            email.as_str()
        );
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::DISPLAY_NAME)
                .await
                .unwrap()
                .as_deref(),
            Some(typed_label.as_ref())
        );
    }
    for (input, expected) in [
        (
            "\"Raoulette Vachette\" <Raoul@Grosbedon.fr>",
            "raoul@grosbedon.fr",
        ),
        ("Raoulette Vachette", "John.Wick@example.com"),
    ] {
        let result = name_create(
            &h,
            input,
            json!({"default_email": "John.Wick@example.com", "default_type": "invalid-type"}),
        )
        .await;
        let id = result[0].as_i64().unwrap();
        let read = call(
            &h,
            "/json/2/res.partner/read",
            &json!({"ids": [id], "fields": ["name", "email", "type"]}).to_string(),
            alice(),
        )
        .await;
        assert_eq!(
            ok(&read),
            json!([{"id": id, "name": "Raoulette Vachette", "email": expected, "type": "contact"}])
        );
        let (typed_id, typed_label) = partners
            .with_context([
                ("default_email", FieldValue::str("John.Wick@example.com")),
                ("default_type", FieldValue::str("invalid-type")),
            ])
            .name_create(&mut tx, input)
            .await
            .unwrap();
        let typed_partner = partners.browse([typed_id]).unwrap();
        assert_eq!(typed_label.as_ref(), "Raoulette Vachette");
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::NAME)
                .await
                .unwrap()
                .as_deref(),
            Some("Raoulette Vachette")
        );
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::EMAIL)
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
        assert_eq!(
            typed_partner
                .get(&mut tx, ResPartner::TYPE)
                .await
                .unwrap()
                .as_deref(),
            Some("contact")
        );
    }
    let before = lookup(&h, json!({"name": "", "limit": 100}))
        .await
        .as_array()
        .unwrap()
        .len();
    let refusal = call(&h, "/json/2/res.partner/name_create", &json!({"name": "Raoul chirurgiens-dentistes.fr", "context": {"force_email": true, "default_email": "default@example.com"}}).to_string(), alice()).await;
    assert_error(
        &refusal,
        422,
        "erp.exceptions.ValidationError",
        "Couldn't create contact without email address!",
    );
    assert_eq!(
        lookup(&h, json!({"name": "", "limit": 100}))
            .await
            .as_array()
            .unwrap()
            .len(),
        before
    );
    let typed_before = partners
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let force_email = partners.with_context([
        ("force_email", FieldValue::Bool(true)),
        ("default_email", FieldValue::str("default@example.com")),
    ]);
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            force_email
                .name_create(tx, "Raoul chirurgiens-dentistes.fr")
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Couldn't create contact without email address!"
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        typed_before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_rpc_binding_and_write_authority_are_preserved() {
    let h = harness().await;
    let invalid = call(
        &h,
        "/json/2/res.partner/name_create",
        r#"{"name": 42}"#,
        alice(),
    )
    .await;
    assert_error(
        &invalid,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'name' of res.partner.name_create: expected a string, got integer",
    );
    let invalid = call(
        &h,
        "/json/2/res.partner/name_search",
        r#"{"name": "X", "operator": "bad_op"}"#,
        alice(),
    )
    .await;
    assert_error(
        &invalid,
        422,
        "erp.exceptions.ValidationError",
        "argument 'operator' of res.partner.name_search: Invalid operator \"bad_op\"",
    );
    let invalid = call(
        &h,
        "/json/2/res.partner/name_search",
        r#"{"name": "X", "limit": -1}"#,
        alice(),
    )
    .await;
    assert_error(
        &invalid,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'limit' of res.partner.name_search: expected a non-negative integer, got -1",
    );
    let denied = call(
        &h,
        "/json/2/res.partner/name_create",
        r#"{"name": "Denied <denied@example.com>"}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &denied,
        403,
        "erp.exceptions.AccessError",
        "Your access rights do not let you create 'Contact' (res.partner) records.",
    );
    assert_eq!(lookup(&h, json!({"name": "Denied"})).await, json!([]));
    let denied = call(
        &h,
        "/json/2/res.partner/name_create",
        r#"{"name": "Denied <denied@example.com>"}"#,
        alice_key(scope(8, "reader", erp_orm::ModelAllow::All, true)),
    )
    .await;
    assert_error(
        &denied,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'res.partner/name_create' (effect 'write').",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_rpc_search_cannot_use_a_forbidden_matching_field() {
    let h = harness_with_policy(
        &[&erp_addon_base::ADDON],
        erp_testkit::FieldPolicy::new().forbid("res.partner", "email"),
        RpcConfig::default(),
        false,
    )
    .await;
    let response = call(
        &h,
        "/json/2/res.partner/name_search",
        r#"{"name": "secret@example.com"}"#,
        alice(),
    )
    .await;
    assert_error(
        &response,
        403,
        "erp.exceptions.AccessError",
        "You do not have enough rights to access the field \"email\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: read",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn label_input_limits_refuse_rpc_create_and_write_without_leaking_contact_text() {
    let h = harness().await;
    let id = create(&h, json!({"name": "Unchanged"})).await;
    for (field, size, message) in [
        (
            "street",
            1_048_577,
            "Contact label input res.partner.street exceeds the 1048576-byte limit",
        ),
        (
            "email",
            65_537,
            "Contact email input exceeds the 65536-byte limit",
        ),
    ] {
        let mut values = serde_json::Map::new();
        values.insert(field.into(), json!("s".repeat(size)));
        values.insert("name".into(), json!("Should roll back"));
        let response = call(
            &h,
            "/json/2/res.partner/write",
            &json!({"ids": [id], "vals": values}).to_string(),
            alice(),
        )
        .await;
        assert_error(&response, 422, "erp.exceptions.ValidationError", message);
        let response = call(
            &h,
            "/json/2/res.partner/create",
            &json!({"vals_list": [{"name": "Valid sibling"}, values]}).to_string(),
            alice(),
        )
        .await;
        assert_error(&response, 422, "erp.exceptions.ValidationError", message);
        let mut tx = h.harness().begin_as(ALICE).await.unwrap();
        let model = tx.env().records("res.partner").unwrap();
        assert_eq!(
            model
                .browse([id])
                .unwrap()
                .value(&mut tx, "name")
                .await
                .unwrap(),
            FieldValue::str("Unchanged")
        );
        assert_eq!(
            model
                .browse([id])
                .unwrap()
                .value(&mut tx, field)
                .await
                .unwrap(),
            FieldValue::False
        );
        assert_eq!(
            model
                .search_count(
                    &mut tx,
                    &Domain::condition("name", Operator::Eq, "Valid sibling").unwrap(),
                    None
                )
                .await
                .unwrap(),
            0
        );
    }
    let denied = call(
        &h,
        "/json/2/res.partner/write",
        &json!({"ids": [id], "vals": {"email": "s".repeat(65_537)}}).to_string(),
        alice_key(scope(8, "reader", erp_orm::ModelAllow::All, true)),
    )
    .await;
    assert_error(
        &denied,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'res.partner/write' (effect 'write').",
    );
    h.cleanup().await.unwrap();
}
