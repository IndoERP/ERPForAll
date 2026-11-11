//! `fields_get` through the JSON-2 dispatch (delta review S-4b): the RPC binding brings the
//! policy and the subject's groups up to date before it asks for the field masks, so a fresh
//! transaction whose principal carries a resolution the caller built (a hint, never trusted when a
//! resolver is installed) answers from the database's memberships.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::Arc;

use erp_orm::{
    Capability, CompanyId, GroupId, GroupSet, Orm, PolicyProvider, Principal, PrincipalBuilder,
    Resolution, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "x_fg",
    depends: &["base"],
    exact_only: false,
};

const THING: &str = "x_fg.thing";

struct Host;

impl RpcHost for Host {
    fn language_active(&self, _code: &str) -> erp_sdk::Result<bool> {
        Ok(false)
    }

    fn has_capability(
        &self,
        _principal: &Principal,
        _capability: &Capability,
    ) -> erp_sdk::Result<bool> {
        Ok(false)
    }
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

async fn group(client: &Client, name: &str, module: &str, local: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    exec(
        client,
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &local, &id],
    )
    .await;
    id
}

async fn new_user(client: &Client, login: &str, groups: &[i64]) -> i64 {
    let uid = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
         RETURNING id::int8",
        &[&login],
    )
    .await;
    for g in groups {
        exec(
            client,
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, g],
        )
        .await;
    }
    uid
}

/// A principal as a session layer could hand it over: the groups are the caller's claim.
fn caller(uid: i64, company: i64, claimed_groups: &[i64]) -> Caller {
    let groups = GroupSet::new(claimed_groups.iter().map(|g| GroupId::new(*g).unwrap()));
    let company = CompanyId::new(company).unwrap();
    Caller::Principal(
        PrincipalBuilder::user(UserId::new(uid).unwrap())
            .resolution(Resolution::in_id_order(groups, [company], NonZeroU64::MIN).unwrap())
            .companies(vec![company])
            .build()
            .unwrap(),
    )
}

async fn field_names(service: &RpcService, who: Caller) -> BTreeSet<String> {
    let response = service
        .call(RpcCall::json2(
            &format!("/json/2/{THING}/fields_get"),
            br#"{"attributes": ["string"]}"#.to_vec(),
            who,
        ))
        .await;
    let text = String::from_utf8_lossy(response.body()).into_owned();
    assert_eq!(response.status(), 200, "{text}");
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    body.as_object().unwrap().keys().cloned().collect()
}

/// user the transaction has not resolved yet; `get_view` is's)
#[tokio::test]
async fn fields_get_rpc_resolves_the_caller_before_the_field_masks() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, &ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("x_fg", THING).description("Thing").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::char("secret").groups("x_fg.group_secret"),
                    ]
                },
            ),
        ])
        .build()
        .await
        .expect("harness");
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", "base", "group_user").await;
    let secret = group(&client, "Secret", "x_fg", "group_secret").await;
    let system = group(&client, "Administrator", "base", "group_system").await;
    new_user(&client, "admin", &[internal, system]).await;
    let model = one(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ('Thing', $1, 'base', false) \
         RETURNING id::int8",
        &[&THING],
    )
    .await;
    exec(
        &client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
         VALUES ('internal', $1, $2, 'crud', true)",
        &[&model, &internal],
    )
    .await;
    let member = new_user(&client, "member", &[internal, secret]).await;
    let plain = new_user(&client, "plain", &[internal]).await;
    let currency = erp_testkit::fixtures::seed_currency(&client).await.unwrap();
    let company = one(
        &client,
        "INSERT INTO res_company (name, currency_id) VALUES ('Main', $1) RETURNING id::int8",
        &[&currency],
    )
    .await;
    for uid in [member, plain] {
        exec(
            &client,
            "UPDATE res_users SET company_id = $2 WHERE id = $1",
            &[&uid, &company],
        )
        .await;
        exec(
            &client,
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&uid, &company],
        )
        .await;
    }

    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool())
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    let service = RpcService::new(orm, Host, RpcConfig::default());

    // the member's memberships are in the database; the caller claims none
    let seen = field_names(&service, caller(member, company, &[])).await;
    assert!(seen.contains("secret"), "member sees {seen:?}");
    assert!(seen.contains("name"), "member sees {seen:?}");
    // the other user's caller claims the group; the database says no
    let seen = field_names(&service, caller(plain, company, &[secret])).await;
    assert!(!seen.contains("secret"), "plain sees {seen:?}");
    assert!(seen.contains("name"), "plain sees {seen:?}");

    drop(service);
    drop(client);
    h.cleanup().await.unwrap();
}
