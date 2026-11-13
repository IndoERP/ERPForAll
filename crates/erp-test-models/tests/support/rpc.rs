#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers"
)]
//! The restricting fixture of the JSON-2 tests (design note §17): users,
//! a policy with read-only and denied models and a company record rule, an API-key scope, a
//! host whose admin holds `rpc.debug_errors`, and call helpers.

use std::num::NonZeroU64;
use std::time::Duration;

use erp_orm::{
    ApiKeyId, Capability, CompanyId, GroupSet, MethodAllow, ModelAllow, Operation, Principal,
    PrincipalBuilder, Resolution, Scope, UserId,
};
use erp_registry::ModelSpec;
use erp_rpc::{Caller, IdempotencyKey, RpcCall, RpcConfig, RpcHost, RpcResponse};
use erp_sdk::domain::Value as DValue;
use erp_sdk::{Domain, Operator};
use erp_sdk::{Error, Result};
use erp_testkit::{Harness, RpcHarness};

/// An internal user with full access to the fixture models.
pub const ALICE: i64 = 2;
/// A user who may only read, and nothing of `test_rpc.model_*`.
pub const BOB: i64 = 3;
/// Like Alice, and holds `rpc.debug_errors` (the system group).
pub const ADMIN: i64 = 4;

/// The fixture policy: never allow-all.
#[derive(Debug, Clone, Copy)]
pub struct FixturePolicy;

impl erp_testkit::TestRules for FixturePolicy {
    fn check_model(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        // a policy failure that is not a refusal (`has_access` must propagate it)
        if model.name == "test_http.galaxy" && op == Operation::Unlink {
            return Err(Error::new(
                erp_sdk::ErrorKind::Database,
                "the policy store is unreachable",
            ));
        }
        let allowed = match b.principal().subject().get() {
            ALICE | ADMIN => true,
            BOB => op == Operation::Read && !model.name.starts_with("test_rpc.model_"),
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(Error::access(format!(
                "You are not allowed to {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )))
        }
    }

    /// `test_rpc.effects` records are readable within the principal's allowed companies (when it
    /// has any): the record rule of design note §17, a read rule (writes, creates and deletes
    /// are decided by the model check alone).
    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        let companies = b.principal().companies().allowed();
        if model.name != "test_rpc.effects" || op != Operation::Read || companies.is_empty() {
            return Ok(Domain::TRUE);
        }
        Ok(Domain::condition(
            "company",
            Operator::In,
            DValue::list(companies.iter().map(|c| DValue::Int(c.get()))),
        )?)
    }
}

/// Active languages besides `en_US`, and the capability holder.
#[derive(Debug, Clone, Copy)]
pub struct FixtureHost;

impl RpcHost for FixtureHost {
    fn language_active(&self, code: &str) -> Result<bool> {
        Ok(code == "fr_FR")
    }

    fn has_capability(&self, principal: &Principal, capability: &Capability) -> Result<bool> {
        Ok(principal.subject().get() == ADMIN && capability.name() == "rpc.debug_errors")
    }
}

pub fn company(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

/// A resolved user principal of companies 1 and 2 with `allowed` active (epoch `epoch`).
pub fn principal_with(uid: i64, allowed: &[i64], epoch: u64) -> PrincipalBuilder {
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::default(),
                [company(1), company(2)],
                NonZeroU64::new(epoch).unwrap(),
            )
            .unwrap(),
        )
        .companies(allowed.iter().map(|c| company(*c)).collect())
}

pub fn user(uid: i64) -> Caller {
    Caller::Principal(principal_with(uid, &[1], 1).build().unwrap())
}

pub fn alice() -> Caller {
    user(ALICE)
}

/// Alice through a `user`-kind API key with `scope`.
pub fn alice_key(scope: Scope) -> Caller {
    Caller::Principal(principal_with(ALICE, &[1], 1).scope(scope).build().unwrap())
}

pub fn scope(key: i64, name: &str, models: ModelAllow, read_only: bool) -> Scope {
    Scope::builder(ApiKeyId::new(key).unwrap(), name)
        .models(models)
        .methods(MethodAllow::All)
        .read_only(read_only)
        .build()
        .unwrap()
}

pub fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::parse(text).unwrap()
}

/// `base_users`, `test_orm` and `test_rpc` (+ `extra`) under the fixture policy, users seeded.
pub async fn harness_with(
    extra: &[&'static erp_sdk::AddonDescriptor],
    config: RpcConfig,
    signal: bool,
) -> RpcHarness {
    harness_with_policy(
        extra,
        erp_testkit::RulesPolicy::new(FixturePolicy).unwrap(),
        config,
        signal,
    )
    .await
}

/// Like `harness_with`, but with a custom access policy provider (e.g. `FieldPolicy`).
pub async fn harness_with_policy<P: erp_orm::IntoPolicyProvider + std::any::Any>(
    extra: &[&'static erp_sdk::AddonDescriptor],
    policy: P,
    config: RpcConfig,
    signal: bool,
) -> RpcHarness {
    let mut addons: Vec<&'static erp_sdk::AddonDescriptor> = vec![
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::test_rpc::ADDON,
    ];
    addons.extend_from_slice(extra);
    let mut builder = RpcHarness::builder(
        Harness::builder(addons).with_static_decls(super::currency_fixture::decls()),
        policy,
    )
    .host(|_orm| FixtureHost)
    .config(config);
    if signal {
        builder = builder.with_signal();
    }
    let h = builder.build().await.expect("rpc harness");
    seed_users(h.harness()).await;
    h
}

pub async fn harness() -> RpcHarness {
    harness_with(&[], RpcConfig::default(), false).await
}

async fn seed_users(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    // Some binaries link full base (required stored share); others use the minimal kernel
    // user declaration. Seed each declared schema explicitly, without an SQL default.
    let has_share = h
        .registry()
        .model("res.users")
        .expect("users fixture")
        .field("share")
        .is_some();
    let insert = if has_share {
        "INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'alice', 'alice', true, false), \
         (3, 'bob', 'bob', true, false), (4, 'admin', 'admin', true, false)"
    } else {
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'alice', 'alice', true), \
         (3, 'bob', 'bob', true), (4, 'admin', 'admin', true)"
    };
    tx.batch(&format!(
        "{insert}; SELECT setval(pg_get_serial_sequence('res_users', 'id'), 4)"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Run raw SQL as `System` and return the first column of every row as text.
pub async fn sql(h: &RpcHarness, query: &str) -> Vec<Option<String>> {
    let mut tx = h.harness().begin().await.unwrap();
    let rows = tx.query(query, &[]).await.unwrap();
    tx.commit().await.unwrap();
    rows.iter().map(|r| r.get::<_, Option<String>>(0)).collect()
}

/// Run several statements (DDL) as `System`.
pub async fn sql_batch(h: &RpcHarness, statements: &str) {
    let mut tx = h.harness().begin().await.unwrap();
    tx.batch(statements).await.unwrap();
    tx.commit().await.unwrap();
}

/// One text value of a query.
pub async fn sql_one(h: &RpcHarness, query: &str) -> String {
    sql(h, query)
        .await
        .into_iter()
        .next()
        .flatten()
        .unwrap_or_else(|| panic!("no value for {query}"))
}

/// Create a `test_rpc.effects` record as `System` and return its id.
pub async fn effect(h: &RpcHarness, name: &str, company: i64) -> i64 {
    sql_one(
        h,
        &format!(
            "INSERT INTO test_rpc_effects (name, company, counter) VALUES ('{name}', {company}, \
             0) RETURNING id::text"
        ),
    )
    .await
    .parse()
    .unwrap()
}

pub async fn count(h: &RpcHarness, table: &str) -> i64 {
    sql_one(h, &format!("SELECT count(*)::text FROM {table}"))
        .await
        .parse()
        .unwrap()
}

pub async fn call(h: &RpcHarness, path: &str, body: &str, caller: Caller) -> RpcResponse {
    h.service()
        .call(RpcCall::json2(path, body.as_bytes().to_vec(), caller))
        .await
}

pub async fn call_keyed(
    h: &RpcHarness,
    path: &str,
    body: &str,
    caller: Caller,
    key: &IdempotencyKey,
) -> RpcResponse {
    h.service()
        .call(RpcCall::json2(path, body.as_bytes().to_vec(), caller).idempotency_key(key.clone()))
        .await
}

pub async fn call_with_deadline(
    h: &RpcHarness,
    path: &str,
    body: &str,
    caller: Caller,
    deadline: Duration,
) -> RpcResponse {
    h.service()
        .call(RpcCall::json2(path, body.as_bytes().to_vec(), caller).deadline(deadline))
        .await
}

/// The body as JSON.
pub fn json(response: &RpcResponse) -> serde_json::Value {
    serde_json::from_slice(response.body())
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {:?}", response.body()))
}

/// A success: status 200 and the body.
#[track_caller]
pub fn ok(response: &RpcResponse) -> serde_json::Value {
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    json(response)
}

#[track_caller]
pub fn assert_error(response: &RpcResponse, status: u16, name: &str, message: &str) {
    let text = String::from_utf8_lossy(response.body()).into_owned();
    assert_eq!(response.status(), status, "{text}");
    let body = json(response);
    assert_eq!(body["name"], name, "{text}");
    assert_eq!(body["message"], message, "{text}");
    let positions: Vec<usize> = [
        "\"name\":",
        "\"message\":",
        "\"arguments\":",
        "\"timestamp\":",
        "\"context\":",
        "\"debug\":",
    ]
    .iter()
    .map(|k| text.find(k).unwrap_or_else(|| panic!("no {k} in {text}")))
    .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "keys out of order: {text}"
    );
    let has_id = body.get("request_id").is_some();
    assert_eq!(
        body.as_object().unwrap().len(),
        6 + usize::from(has_id),
        "exactly six keys, plus request_id when the call had an id: {text}"
    );
    if has_id {
        assert!(
            text.find("\"debug\":").unwrap() < text.find("\"request_id\":").unwrap(),
            "request_id comes after debug: {text}"
        );
    }
}

/// [`harness`] on an ORM with signals and a read pool whose connections carry
/// `application_name=erp_read_pool`.
pub async fn harness_with_read_pool() -> RpcHarness {
    let builder = RpcHarness::builder(
        Harness::builder([
            &erp_test_models::base_users::ADDON,
            &erp_test_models::test_orm::ADDON,
            &erp_test_models::test_rpc::ADDON,
        ])
        .with_static_decls(super::currency_fixture::decls()),
        erp_testkit::RulesPolicy::new(FixturePolicy).unwrap(),
    )
    .host(|_orm| FixtureHost)
    .config(RpcConfig::default())
    .with_signal()
    .with_read_pool("-c application_name=erp_read_pool");
    let h = builder.build().await.expect("rpc harness");
    seed_users(h.harness()).await;
    h
}
