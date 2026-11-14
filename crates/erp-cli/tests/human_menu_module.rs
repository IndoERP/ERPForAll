// SPDX-License-Identifier: MIT OR Apache-2.0
//! Menu module provenance through installed policy and the real RPC.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
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
async fn xid(tx: &mut Transaction, name: &str, model: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module='base' AND name=$1 AND model=$2",
        &[&name, &model],
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
async fn user(tx: &mut Transaction, name: &str, company: i64, ids: &[i64]) -> i64 {
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
            .set_commands("group_ids", [groups(ids)]),
    )
    .await
}
async fn action(tx: &mut Transaction, name: &str, model: &str, group: Option<i64>) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("res_model", Cell::Str(model.into()))
        .set_cell("view_mode", Cell::Str("form".into()));
    if let Some(group) = group {
        vals = vals.set_commands("group_ids", [groups(&[group])]);
    }
    create(tx, "ir.actions.act_window", vals).await
}

async fn provenance(tx: &mut Transaction, module: &str, name: &str, model: &str, id: i64) -> i64 {
    create(
        tx,
        "ir.model.data",
        RawVals::new()
            .set_cell("module", Cell::Str(module.into()))
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("model", Cell::Str(model.into()))
            .set_cell("res_id", Cell::Id(id)),
    )
    .await
}
async fn menu(tx: &mut Transaction, name: &str, action: Option<i64>, parent: Option<i64>) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("active", Cell::Bool(true))
        .set_cell("web_icon", Cell::Str("lucide:box".into()));
    if let Some(id) = action {
        vals = vals.set_cell(
            "action",
            Cell::Str(format!("ir.actions.act_window,{id}").into()),
        );
    }
    if let Some(id) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(id));
    }
    create(tx, "ir.ui.menu", vals).await
}
fn node(tree: &Value, id: i64) -> &Value {
    tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id)
        .unwrap()
}
async fn menus(service: &RpcService, principal: &erp_orm::Principal, stage: &str) -> Value {
    let response = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(principal.clone()),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "stage={stage}: {}",
        String::from_utf8_lossy(response.body())
    );
    serde_json::from_slice(response.body()).unwrap()
}

#[test]
fn menu_module_provenance_is_authorized_bounded_and_read_only() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(run());
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn run() {
    let db = TestDb::create().await.unwrap();
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

    let internal = xid(&mut tx, "group_user", "res.groups").await;
    let regular = xid(&mut tx, "group_user_regular", "res.groups").await;
    let system_group = xid(&mut tx, "group_system", "res.groups").await;
    let company = xid(&mut tx, "main_company", "res.company").await;
    let admin = user(
        &mut tx,
        "menu-module-admin",
        company,
        &[internal, system_group],
    )
    .await;
    let ordinary = user(&mut tx, "menu-module-user", company, &[internal, regular]).await;
    let excluded = create(
        &mut tx,
        "res.groups",
        RawVals::new().set_cell("name", Cell::Str("Excluded menu group".into())),
    )
    .await;
    let readable = action(&mut tx, "Readable", "res.partner", None).await;
    let denied_action = action(&mut tx, "Denied action", "res.partner", Some(excluded)).await;
    let denied_model = action(&mut tx, "Denied model", "res.groups", None).await;
    let root = menu(&mut tx, "Same label", None, None).await;
    let child = menu(&mut tx, "Same label", Some(readable), Some(root)).await;
    let manual = menu(&mut tx, "Manual", Some(readable), None).await;
    let empty = menu(&mut tx, "Empty namespace", Some(readable), None).await;
    let first = provenance(&mut tx, "z_owner", "root", "ir.ui.menu", root).await;
    let later = provenance(&mut tx, &"a".repeat(4097), "root", "ir.ui.menu", root).await;
    assert!(
        first < later,
        "the fixture must distinguish row order from lexical order"
    );
    provenance(&mut tx, "child_owner", "child", "ir.ui.menu", child).await;
    provenance(&mut tx, "", "empty_namespace", "ir.ui.menu", empty).await;
    // A polymorphic provenance ID can collide with an unrelated model's ID.
    tx.execute("INSERT INTO ir_model_data (module,name,model,res_id,noupdate) VALUES ('foreign','menu_id_collision','res.partner',$1,false)", &[&manual]).await.unwrap();
    let hidden = menu(&mut tx, "Hidden menu", Some(readable), None).await;
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([hidden])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [groups(&[excluded])]),
        )
        .await
        .unwrap();
    let hidden_provenance =
        provenance(&mut tx, &"h".repeat(4097), "hidden", "ir.ui.menu", hidden).await;
    let pruned = menu(&mut tx, "Pruned folder", None, None).await;
    let denied = menu(&mut tx, "Denied action", Some(denied_action), Some(pruned)).await;
    let denied_target = menu(&mut tx, "Denied model", Some(denied_model), None).await;
    provenance(&mut tx, "denied_owner", "denied", "ir.ui.menu", denied).await;
    provenance(
        &mut tx,
        "denied_owner",
        "denied_target",
        "ir.ui.menu",
        denied_target,
    )
    .await;
    // Assert a real installed root, independent of its translated presentation name.
    let installed = tx.query("SELECT d.res_id, d.module FROM ir_model_data d JOIN ir_ui_menu m ON m.id=d.res_id WHERE d.model='ir.ui.menu' AND d.module='base' AND m.parent_id IS NULL AND m.action IS NOT NULL ORDER BY d.id LIMIT 1", &[]).await.unwrap();
    assert_eq!(installed.len(), 1);
    let installed_id: i64 = installed[0].get(0);
    let installed_module: String = installed[0].get(1);
    let before: i64 = tx
        .query("SELECT count(*)::bigint FROM ir_model_data", &[])
        .await
        .unwrap()[0]
        .get(0);
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
    let mut tx = orm
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    let principal = tx.env().principal().clone();
    let error = tx
        .env()
        .records("ir.model.data")
        .unwrap()
        .check_access(&mut tx, erp_orm::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you access 'Model Data' (ir.model.data) records.\n\nGroups that may do this:\n\t- Settings Administration\n\nAsk an administrator if you need this access."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // Commit/rollback ends SQL work; drop returns the checkout before opening an RPC unit.
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::for_principal(principal.clone())
                .read_only("ir.ui.menu.erp_load_web_menus"),
        )
        .await
        .unwrap();
    let env = tx.env();
    let produced =
        erp_addon_web::metadata_test_support::test_load_menus(&env, &mut tx, 32 * 1024 * 1024)
            .await
            .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    let tree = menus(&service, &principal, "initial").await;
    assert_eq!(serde_json::to_value(produced).unwrap(), tree);
    assert_eq!(node(&tree, root).get("module"), Some(&json!("z_owner")));
    assert_eq!(
        node(&tree, child).get("module"),
        Some(&json!("child_owner"))
    );
    assert_eq!(node(&tree, manual).get("module"), Some(&Value::Null));
    assert_eq!(node(&tree, empty).get("module"), Some(&Value::Null));
    assert_eq!(node(&tree, root)["label"], "Same label");
    assert_eq!(node(&tree, child)["icon"], "lucide:box");
    assert_eq!(node(&tree, child)["action_id"], readable);
    assert_eq!(node(&tree, root)["children"], json!([child]));
    // Exercise wire admission against the real producer's response, not a literal fixture.
    let mut missing = tree.clone();
    missing["nodes"][0]
        .as_object_mut()
        .unwrap()
        .remove("module");
    let error = serde_json::from_value::<erp_sdk::views::wire::MenuTree>(missing).unwrap_err();
    assert!(error.is_data());
    assert_eq!(error.to_string(), "missing field `module`");
    let mut wrong_type = tree.clone();
    wrong_type["nodes"][0]["module"] = json!(true);
    let error = serde_json::from_value::<erp_sdk::views::wire::MenuTree>(wrong_type).unwrap_err();
    assert!(error.is_data());
    assert_eq!(
        error.to_string(),
        "invalid type: boolean `true`, expected a string"
    );
    for id in [hidden, pruned, denied, denied_target] {
        assert!(
            !tree["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == id)
        );
    }
    assert_eq!(menus(&service, &principal, "repeat").await, tree);
    // Admin sees the installed base root; remove the deliberately oversized hidden fixture first.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let after: i64 = tx
        .query("SELECT count(*)::bigint FROM ir_model_data", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        after, before,
        "metadata reads must never create external IDs"
    );
    tx.env()
        .records("ir.model.data")
        .unwrap()
        .browse([hidden_provenance])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("module", Cell::Str("hidden_owner".into())),
        )
        .await
        .unwrap();
    tx.env()
        .records("ir.model.data")
        .unwrap()
        .browse([first])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("module", Cell::Str("changed_owner".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let changed = menus(&service, &principal, "changed").await;
    assert_eq!(node(&changed, root)["module"], "changed_owner");
    assert_eq!(changed["roots"], tree["roots"]);
    let mut tx = orm
        .begin(erp_orm::UserId::new(admin).unwrap())
        .await
        .unwrap();
    let admin_principal = tx.env().principal().clone();
    tx.rollback().await.unwrap();
    drop(tx);
    let admin_tree = menus(&service, &admin_principal, "admin").await;
    assert_eq!(node(&admin_tree, installed_id)["module"], installed_module);
    // Reject module text before reading it; no successful DTO prefix can escape.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let oversized = "é".repeat(2049);
    tx.env()
        .records("ir.model.data")
        .unwrap()
        .browse([first])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("module", Cell::Str(oversized.into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::for_principal(admin_principal.clone())
                .read_only("ir.ui.menu.erp_load_web_menus"),
        )
        .await
        .unwrap();
    let env = tx.env();
    let (result, stages) = erp_addon_web::metadata_test_support::test_load_menus_probed(
        &env,
        &mut tx,
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        error.kind(),
        erp_fields::ErrorKind::Validation,
        "{}",
        error.message()
    );
    assert_eq!(error.message(), "Menu module exceeds the 4096-byte limit.");
    assert!(!stages.contains(&"menus:modules:read"));
    tx.rollback().await.unwrap();
    drop(tx);
    // This admitted fixture has large module text but tiny menu labels, isolating provenance storage.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.model.data")
        .unwrap()
        .browse([first])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("module", Cell::Str("changed_owner".into())),
        )
        .await
        .unwrap();
    for i in 0..20 {
        let id = menu(&mut tx, &format!("Budget {i}"), Some(readable), None).await;
        provenance(
            &mut tx,
            &"m".repeat(4096),
            &format!("budget_{i}"),
            "ir.ui.menu",
            id,
        )
        .await;
    }
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::for_principal(admin_principal.clone())
                .read_only("ir.ui.menu.erp_load_web_menus"),
        )
        .await
        .unwrap();
    let env = tx.env();
    let (result, stages) =
        erp_addon_web::metadata_test_support::test_load_menus_probed(&env, &mut tx, 250_000)
            .await
            .unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        error.kind(),
        erp_fields::ErrorKind::Validation,
        "{}",
        error.message()
    );
    // The reached byte count depends on platform container sizes; the budget/kind/message
    // prefix are stable, and the reported numeric count must exceed the admitted maximum.
    let (message, reached) = error.message().split_once(", reached ").unwrap();
    assert_eq!(
        message,
        "simultaneously retained description bytes limit exceeded: maximum 250000"
    );
    assert!(reached.parse::<usize>().unwrap() > 250_000);
    assert!(stages.contains(&"menus:modules:reserve"), "{stages:?}");
    assert!(!stages.contains(&"menus:modules:read"));
    tx.rollback().await.unwrap();
    drop(tx);
    drop(service);
    drop(orm);
    drop(loaded);
    drop(system);
    db.cleanup().await.unwrap();
}
