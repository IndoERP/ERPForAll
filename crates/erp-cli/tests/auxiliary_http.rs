// SPDX-License-Identifier: MIT OR Apache-2.0
//! Contextual bindings and favourites Read RPC integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;
use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use serde_json::{Value, json};

struct Host;
impl RpcHost for Host {
    fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
        Ok(code == "en_US")
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        Ok(false)
    }
}

async fn xid(tx: &mut Transaction, module: &str, name: &str, model: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module=$1 AND name=$2 AND model=$3",
        &[&module, &name, &model],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

fn groups(ids: &[i64]) -> Command {
    Command::set(ids.iter().map(|id| RecId::from_real(*id).unwrap()))
}

async fn make_user(tx: &mut Transaction, name: &str, company: i64, group_ids: &[i64]) -> i64 {
    let partner = create(
        tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("company_id", Cell::Id(company)),
    )
    .await;
    create(
        tx,
        "res.users",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("login", Cell::Str(name.into()))
            .set_cell("partner_id", Cell::Id(partner))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("lang", Cell::Str("en_US".into()))
            .set_cell("tz", Cell::Str("UTC".into()))
            .set_commands("company_ids", [groups(&[company])])
            .set_commands("group_ids", [groups(group_ids)]),
    )
    .await
}

async fn rpc(
    service: &RpcService,
    principal: &erp_orm::Principal,
    path: &str,
    params: Value,
) -> (u16, Value) {
    let body = serde_json::to_vec(&params).unwrap();
    let response = service
        .call(RpcCall::json2(
            path,
            body,
            Caller::Principal(principal.clone()),
        ))
        .await;
    let status = response.status();
    let body: Value = serde_json::from_slice(response.body()).unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn auxiliary_bindings_and_filters_rpc_contract() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_test());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_test() {
    // the policy source, the two ORMs and the RPC calls each hold a connection
    let db = TestDb::builder().pool_size(8).create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let loaded = system.initialize(InitOptions::new()).await.unwrap();
    assert!(loaded.post_commit.is_clean());

    let mut tx = loaded.orm.begin(SUPERUSER_ID).await.unwrap();

    let internal = xid(&mut tx, "base", "group_user", "res.groups").await;
    let regular = xid(&mut tx, "base", "group_user_regular", "res.groups").await;
    let system_group = xid(&mut tx, "base", "group_system", "res.groups").await;
    let company = xid(&mut tx, "base", "main_company", "res.company").await;

    // The administrator comes first: a database without an active administrator refuses every
    // change of memberships (the last-administrator rule).
    let admin_user = make_user(
        &mut tx,
        "aux-admin",
        company,
        &[internal, regular, system_group],
    )
    .await;
    // User A and User B share the exact same groups
    let user_a = make_user(&mut tx, "aux-user-a", company, &[internal, regular]).await;
    let user_b = make_user(&mut tx, "aux-user-b", company, &[internal, regular]).await;

    // Seed target model records
    let partner_model_id = xid(&mut tx, "base", "model_res_partner", "ir.model").await;

    // Contextual bindings setup:
    // Action 1: bound to res.partner, view_types = "list,form", no group
    let action_1 = create(
        &mut tx,
        "ir.actions.act_window",
        RawVals::new()
            .set_cell("name", Cell::Str("Partner Action ListForm".into()))
            .set_cell("res_model", Cell::Str("res.partner".into()))
            .set_cell("view_mode", Cell::Str("list,form".into()))
            .set_cell("binding_model_id", Cell::Id(partner_model_id))
            .set_cell("binding_type", Cell::Str("action".into()))
            .set_cell("binding_view_types", Cell::Str("list,form".into())),
    )
    .await;

    // Action 2: bound to res.partner, view_types = "form", group restricted to system_group
    let _action_2 = create(
        &mut tx,
        "ir.actions.act_window",
        RawVals::new()
            .set_cell("name", Cell::Str("Partner Action Form AdminOnly".into()))
            .set_cell("res_model", Cell::Str("res.partner".into()))
            .set_cell("view_mode", Cell::Str("form".into()))
            .set_cell("binding_model_id", Cell::Id(partner_model_id))
            .set_cell("binding_type", Cell::Str("action".into()))
            .set_cell("binding_view_types", Cell::Str("form".into()))
            .set_commands("group_ids", [groups(&[system_group])]),
    )
    .await;

    // Report action: bound to res.partner, binding_type = "report"
    let _report_1 = create(
        &mut tx,
        "ir.actions.act_window",
        RawVals::new()
            .set_cell("name", Cell::Str("Partner Report".into()))
            .set_cell("res_model", Cell::Str("res.partner".into()))
            .set_cell("view_mode", Cell::Str("form".into()))
            .set_cell("binding_model_id", Cell::Id(partner_model_id))
            .set_cell("binding_type", Cell::Str("report".into()))
            .set_cell("binding_view_types", Cell::Str("list".into())),
    )
    .await;

    // Favourites / ir.filters setup:
    // Filter 1: Public filter on res.partner (user_ids empty), global action (action_id NULL)
    let filter_pub = create(
        &mut tx,
        "ir.filters",
        RawVals::new()
            .set_cell("name", Cell::Str("Public Customers".into()))
            .set_cell("model_id", Cell::Str("res.partner".into()))
            .set_cell("domain", Cell::Str("[('is_company', '=', True)]".into()))
            .set_cell("context", Cell::Str("{}".into()))
            .set_cell("sort", Cell::Str("[]".into()))
            .set_cell("is_default", Cell::Bool(false))
            .set_cell("active", Cell::Bool(true)),
    )
    .await;

    // Filter 2: Private filter for User A on res.partner (user_ids = [user_a])
    let filter_user_a = create(
        &mut tx,
        "ir.filters",
        RawVals::new()
            .set_cell("name", Cell::Str("User A Private Leads".into()))
            .set_cell("model_id", Cell::Str("res.partner".into()))
            .set_commands("user_ids", [groups(&[user_a])])
            .set_cell("domain", Cell::Str("[('active', '=', True)]".into()))
            .set_cell("context", Cell::Str("{}".into()))
            .set_cell("sort", Cell::Str("[]".into()))
            .set_cell("is_default", Cell::Bool(false))
            .set_cell("active", Cell::Bool(true)),
    )
    .await;

    // Filter 3: Action-scoped filter for Action 1
    let filter_action_scoped = create(
        &mut tx,
        "ir.filters",
        RawVals::new()
            .set_cell("name", Cell::Str("Action 1 Scoped Filter".into()))
            .set_cell("model_id", Cell::Str("res.partner".into()))
            .set_cell("action_id", Cell::Id(action_1))
            .set_cell("domain", Cell::Str("[]".into()))
            .set_cell("context", Cell::Str("{}".into()))
            .set_cell("sort", Cell::Str("[]".into()))
            .set_cell("is_default", Cell::Bool(true))
            .set_cell("active", Cell::Bool(true)),
    )
    .await;

    // Commit setup
    tx.commit().await.unwrap();
    drop(tx);

    let source = PolicySource::build(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let table = loaded
        .orm
        .env(SUPERUSER_ID, Context::new())
        .generation_attachment::<erp_sdk::views::PublishedViewTypes>()
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(loaded.orm.registry()).unwrap())
        .policy(provider)
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(loaded.orm.registry()),
            table,
        ))
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();

    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());

    // Resolve principals
    let mut tx_a = orm
        .begin(erp_orm::UserId::new(user_a).unwrap())
        .await
        .unwrap();
    let principal_a = tx_a.env().principal().clone();
    tx_a.rollback().await.unwrap();

    let mut tx_b = orm
        .begin(erp_orm::UserId::new(user_b).unwrap())
        .await
        .unwrap();
    let principal_b = tx_b.env().principal().clone();
    tx_b.rollback().await.unwrap();

    let mut tx_admin = orm
        .begin(erp_orm::UserId::new(admin_user).unwrap())
        .await
        .unwrap();
    let principal_admin = tx_admin.env().principal().clone();
    tx_admin.rollback().await.unwrap();

    // 1.: rejects unknown model
    let (status, err) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.actions.actions/erp_get_bindings",
        json!({"model": "unknown.model", "view_type": null}),
    )
    .await;
    assert_ne!(status, 200);
    assert_eq!(err["name"], "erp.exceptions.ValidationError");

    // 2.: User A calling for "list" view_type on "res.partner"
    let (status, resp) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.actions.actions/erp_get_bindings",
        json!({"model": "res.partner", "view_type": "list"}),
    )
    .await;
    assert_eq!(status, 200, "resp: {resp}");
    let actions = resp["action"].as_array().expect("action array");
    let prints = resp["print"].as_array().expect("print array");

    // Action 1 matches "list,form"; Action 2 requires system_group (user A lacks it)
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["id"], action_1);
    assert_eq!(actions[0]["name"], "Partner Action ListForm");

    // Report 1 has view_type "list"
    assert_eq!(prints.len(), 1);
    assert_eq!(prints[0]["name"], "Partner Report");

    // 3. User A calling for "search" view_type: Action 1 and Report 1 don't match "search"
    let (status, resp_search) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.actions.actions/erp_get_bindings",
        json!({"model": "res.partner", "view_type": "search"}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(resp_search["action"].as_array().unwrap().is_empty());
    assert!(resp_search["print"].as_array().unwrap().is_empty());

    // 4. Admin calling for "form": matches Action 1 ("list,form") and Action 2 ("form", system_group)
    let (status, resp_admin) = rpc(
        &service,
        &principal_admin,
        "/json/2/ir.actions.actions/erp_get_bindings",
        json!({"model": "res.partner", "view_type": "form"}),
    )
    .await;
    assert_eq!(status, 200);
    let admin_actions = resp_admin["action"].as_array().unwrap();
    // Both action 1 and action 2
    assert_eq!(admin_actions.len(), 2);
    // window actions have no sequence, so bindings come in id order: Action 1 then Action 2
    assert_eq!(admin_actions[0]["name"], "Partner Action ListForm");
    assert_eq!(admin_actions[1]["name"], "Partner Action Form AdminOnly");

    // 5. Test alias get_bindings
    let (status, resp_alias) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.actions.actions/get_bindings",
        json!({"model": "res.partner", "view_type": "list"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(resp_alias, resp);

    // 1.: Constraint: embedded_parent_res_id without embedded_action_id
    let (status, err) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/get_filters",
        json!({
            "model": "res.partner",
            "action_id": null,
            "embedded_action_id": null,
            "embedded_parent_res_id": 42
        }),
    )
    .await;
    assert_ne!(status, 200);
    assert_eq!(err["name"], "erp.exceptions.ValidationError");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("embedded_parent_res_id")
    );

    // 2.: Unknown model
    let (status, err) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/get_filters",
        json!({
            "model": "nonexistent.model",
            "action_id": null,
            "embedded_action_id": null,
            "embedded_parent_res_id": null
        }),
    )
    .await;
    assert_ne!(status, 200);
    assert_eq!(err["name"], "erp.exceptions.ValidationError");

    // 3.: Subject Privacy Isolation
    // User A receives Public filter AND User A's private filter
    let (status, filters_a) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/get_filters",
        json!({
            "model": "res.partner",
            "action_id": null,
            "embedded_action_id": null,
            "embedded_parent_res_id": null
        }),
    )
    .await;
    assert_eq!(status, 200, "filters_a: {filters_a}");
    let list_a = filters_a.as_array().expect("array of filters");
    let names_a: Vec<&str> = list_a.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names_a.contains(&"Public Customers"));
    assert!(names_a.contains(&"User A Private Leads"));

    // User B (identical groups to User A) receives Public filter, but NEVER User A's private filter!
    let (status, filters_b) = rpc(
        &service,
        &principal_b,
        "/json/2/ir.filters/get_filters",
        json!({
            "model": "res.partner",
            "action_id": null,
            "embedded_action_id": null,
            "embedded_parent_res_id": null
        }),
    )
    .await;
    assert_eq!(status, 200, "filters_b: {filters_b}");
    let list_b = filters_b.as_array().expect("array of filters");
    let names_b: Vec<&str> = list_b.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names_b.contains(&"Public Customers"));
    assert!(
        !names_b.contains(&"User A Private Leads"),
        "User B MUST NOT receive User A private filter!"
    );

    let mut tx_b = orm
        .begin(erp_orm::UserId::new(user_b).unwrap())
        .await
        .unwrap();
    let visible: Vec<i64> = tx_b
        .env()
        .records("ir.filters")
        .unwrap()
        .search(
            &mut tx_b,
            &erp_sdk::Domain::condition("model_id", erp_sdk::domain::Operator::Eq, "res.partner")
                .unwrap(),
        )
        .await
        .unwrap()
        .ids()
        .to_vec();
    tx_b.rollback().await.unwrap();
    assert!(visible.contains(&filter_pub), "shared filters are readable");
    assert!(
        !visible.contains(&filter_user_a),
        "a plain search must not return another user's private filter"
    );

    // 4. Action-scoping: when action_id = action_1, includes global filters AND action_1 scoped filters
    let (status, filters_action) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/get_filters",
        json!({
            "model": "res.partner",
            "action_id": action_1,
            "embedded_action_id": null,
            "embedded_parent_res_id": null
        }),
    )
    .await;
    assert_eq!(status, 200, "filters_action: {filters_action}");
    let list_act = filters_action.as_array().unwrap();
    let names_act: Vec<&str> = list_act
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names_act.contains(&"Public Customers"));
    assert!(names_act.contains(&"User A Private Leads"));
    assert!(names_act.contains(&"Action 1 Scoped Filter"));

    // 5. Test alias erp_get_filters
    let (status, filters_alias) = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/erp_get_filters",
        json!({
            "model": "res.partner",
            "action_id": action_1,
            "embedded_action_id": null,
            "embedded_parent_res_id": null
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(filters_alias, filters_action);

    // 6.: Prove warm description cache in get_views is unchanged before and after
    let mut tx_warm = orm
        .begin(erp_orm::UserId::new(user_a).unwrap())
        .await
        .unwrap();
    let env_warm = tx_warm.env().clone();
    let view_form = xid(&mut tx_warm, "base", "partner_form", "ir.ui.view").await;

    let desc_before = erp_addon_web::metadata_test_support::test_get_views(
        &env_warm,
        &mut tx_warm,
        "res.partner",
        vec![erp_fields::Value::list([
            erp_fields::Value::Int(view_form),
            erp_fields::Value::str("form"),
        ])],
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    tx_warm.commit().await.unwrap();

    // Call auxiliary RPCs multiple times
    let _ = rpc(
        &service,
        &principal_a,
        "/json/2/ir.actions.actions/erp_get_bindings",
        json!({"model": "res.partner"}),
    )
    .await;
    let _ = rpc(
        &service,
        &principal_a,
        "/json/2/ir.filters/get_filters",
        json!({"model": "res.partner"}),
    )
    .await;

    let mut tx_after = orm
        .begin(erp_orm::UserId::new(user_a).unwrap())
        .await
        .unwrap();
    let env_after = tx_after.env().clone();
    let desc_after = erp_addon_web::metadata_test_support::test_get_views(
        &env_after,
        &mut tx_after,
        "res.partner",
        vec![erp_fields::Value::list([
            erp_fields::Value::Int(view_form),
            erp_fields::Value::str("form"),
        ])],
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    tx_after.rollback().await.unwrap();

    let v_before = serde_json::to_value(desc_before).unwrap();
    let v_after = serde_json::to_value(desc_after).unwrap();
    assert_eq!(
        v_before["version"], v_after["version"],
        "Description version must remain identical!"
    );
    assert_eq!(
        v_before["views"], v_after["views"],
        "View descriptions must remain identical!"
    );

    // Deleting an action removes the filters scoped to it through the foreign key's cascade, with
    // no clean-up statement of its own in the action unlink.
    let mut tx_del = loaded.orm.begin(SUPERUSER_ID).await.unwrap();
    tx_del
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([action_1])
        .unwrap()
        .unlink(&mut tx_del)
        .await
        .unwrap();
    let left = tx_del
        .query(
            "SELECT count(*) FROM ir_filters WHERE id = $1",
            &[&filter_action_scoped],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    tx_del.rollback().await.unwrap();
    assert_eq!(
        left, 0,
        "the filter scoped to a deleted action goes with it"
    );
}
