//! Authorized one2many count RPC (ERPForAll extension).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;
use erp_orm::{FieldOp, Operation, PrincipalBinding};
use erp_registry::ModelSpec;
use erp_rpc::RpcConfig;
use erp_sdk::{Domain, Error, Operator, Result};
use erp_test_models::test_access;
use erp_testkit::{FieldPolicy, RulesPolicy};
use serde_json::json;
use support::rpc::*;

const PATH: &str = "/json/2/test_access.shelf/relation_count";
async fn seed(h: &erp_testkit::RpcHarness) {
    sql_batch(h, "INSERT INTO test_access_shelf (id,name) VALUES (1,'visible'),(2,'hidden'),(3,'empty'); INSERT INTO test_access_book (name,val,active,shelf_id) VALUES ('a',1,true,1),('b',2,true,1),('archived',1,false,1),('hidden',1,true,2)").await;
}
#[derive(Clone, Copy)]
struct Rights;
impl erp_testkit::TestRules for Rights {
    fn check_model(&self, b: &PrincipalBinding<'_>, m: &ModelSpec, _: Operation) -> Result<()> {
        if m.name == "test_access.book" && b.principal().subject().get() == BOB {
            return Err(Error::access("book model denied"));
        }
        Ok(())
    }
    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        m: &ModelSpec,
        _: Operation,
    ) -> Result<Domain> {
        match m.name {
            "test_access.shelf" => Ok(Domain::condition("name", Operator::Ne, "hidden")?),
            "test_access.book" => Ok(Domain::condition(
                "val",
                Operator::In,
                erp_sdk::domain::Value::list(
                    b.principal()
                        .companies()
                        .allowed()
                        .iter()
                        .map(|c| erp_sdk::domain::Value::Int(c.get())),
                ),
            )?),
            _ => Ok(Domain::TRUE),
        }
    }
}

#[tokio::test]
async fn count_uses_record_company_active_rules_and_observes_updates() {
    let h = harness_with_policy(
        &[&test_access::ADDON],
        RulesPolicy::new(Rights).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await;
    seed(&h).await;
    for (body, expected) in [
        (json!({"ids":[1],"field":"book_ids"}), 1),
        (
            json!({"ids":[1],"field":"book_ids","context":{"allowed_company_ids":[1,2]}}),
            2,
        ),
        (
            json!({"ids":[1],"field":"book_ids","context":{"active_test":false}}),
            2,
        ),
        (json!({"ids":[3],"field":"book_ids"}), 0),
    ] {
        assert_eq!(
            ok(&call(&h, PATH, &body.to_string(), alice()).await),
            json!(expected)
        );
    }
    sql_batch(
        &h,
        "INSERT INTO test_access_book (name,val,active,shelf_id) VALUES ('new',1,true,1)",
    )
    .await;
    assert_eq!(
        ok(&call(&h, PATH, r#"{"ids":[1],"field":"book_ids"}"#, alice()).await),
        json!(2)
    );
    sql_batch(&h, "DELETE FROM test_access_book WHERE name='new'").await;
    assert_eq!(
        ok(&call(&h, PATH, r#"{"ids":[1],"field":"book_ids"}"#, alice()).await),
        json!(1)
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn denied_is_an_error_including_empty_target() {
    let h = harness_with_policy(
        &[&test_access::ADDON],
        RulesPolicy::new(Rights).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await;
    seed(&h).await;
    for id in [1, 3] {
        assert_error(
            &call(
                &h,
                PATH,
                &json!({"ids":[id],"field":"book_ids"}).to_string(),
                user(BOB),
            )
            .await,
            403,
            "erp.exceptions.AccessError",
            "book model denied",
        );
    }
    let denied = call(&h, PATH, r#"{"ids":[2],"field":"book_ids"}"#, alice()).await;
    assert_eq!(denied.status(), 403, "{}", json(&denied));
    assert_eq!(json(&denied)["name"], "erp.exceptions.AccessError");
    assert!(
        json(&denied)["message"]
            .as_str()
            .unwrap()
            .contains("test_access.shelf")
    );
    assert_eq!(
        ok(&call(&h, PATH, r#"{"ids":[3],"field":"book_ids"}"#, alice()).await),
        json!(0)
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn field_and_source_acl_are_checked_even_at_zero() {
    for policy in [
        FieldPolicy::new().deny("test_access.shelf", "book_ids", FieldOp::Read),
        FieldPolicy::new().deny("test_access.book", "shelf_id", FieldOp::Read),
        FieldPolicy::new().deny_model("test_access.shelf"),
    ] {
        let h =
            harness_with_policy(&[&test_access::ADDON], policy, RpcConfig::default(), false).await;
        seed(&h).await;
        let denied = call(&h, PATH, r#"{"ids":[3],"field":"book_ids"}"#, alice()).await;
        assert_eq!(denied.status(), 403, "{}", json(&denied));
        assert_eq!(json(&denied)["name"], "erp.exceptions.AccessError");
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_inputs_never_become_zero() {
    let h = harness_with_policy(
        &[&test_access::ADDON],
        RulesPolicy::new(Rights).unwrap(),
        RpcConfig::default(),
        false,
    )
    .await;
    seed(&h).await;
    for (body, message) in [
        (
            json!({"ids":[],"field":"book_ids"}),
            "relation_count requires one persisted record",
        ),
        (
            json!({"ids":[1,3],"field":"book_ids"}),
            "relation_count requires one persisted record",
        ),
        (
            json!({"ids":[1],"field":"missing"}),
            "Unknown relation_count field 'test_access.shelf.missing'",
        ),
        (
            json!({"ids":[1],"field":"name"}),
            "relation_count supports only direct one2many fields",
        ),
    ] {
        assert_error(
            &call(&h, PATH, &body.to_string(), alice()).await,
            422,
            "erp.exceptions.ValidationError",
            message,
        );
    }
    let missing = call(&h, PATH, r#"{"ids":[9999],"field":"book_ids"}"#, alice()).await;
    assert_error(
        &missing,
        404,
        "erp.exceptions.MissingError",
        "relation_count owner 'test_access.shelf(9999)' does not exist or is not readable",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unsupported_relation_semantics_refuse_explicitly() {
    use erp_registry::{FieldDecl, ModelDecl};
    use erp_testkit::{Harness, RpcHarness};
    let h = RpcHarness::builder(
        Harness::builder([&erp_test_models::base_users::ADDON, &test_access::ADDON]).with_decls([
            ModelDecl::new("test_access", "count.owner").fields(
                const {
                    &[
                        FieldDecl::one2many("filtered", "count.line", "owner_id").domain("[]"),
                        FieldDecl::one2many("contextual", "count.line", "owner_id").context("{}"),
                        FieldDecl::one2many("nonstored", "count.line", "owner_id").store(false),
                        FieldDecl::many2many("many", "count.line"),
                    ]
                },
            ),
            ModelDecl::new("test_access", "count.line")
                .fields(const { &[FieldDecl::many2one("owner_id", "count.owner")] }),
        ]),
        RulesPolicy::new(Rights).unwrap(),
    )
    .host(|_| FixtureHost)
    .build()
    .await
    .unwrap();
    sql_batch(&h, "INSERT INTO count_owner (id) VALUES (1)").await;
    for field in ["filtered", "contextual", "nonstored", "many"] {
        let message = if field == "many" {
            "relation_count supports only direct one2many fields"
        } else {
            "relation_count does not support nonstored, computed, related, inherited, domain or context one2many fields"
        };
        assert_error(
            &call(
                &h,
                "/json/2/count.owner/relation_count",
                &json!({"ids":[1],"field":field}).to_string(),
                alice(),
            )
            .await,
            422,
            "erp.exceptions.ValidationError",
            message,
        );
    }
    h.cleanup().await.unwrap();
}
