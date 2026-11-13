//! UoM views, window action, settings menu and access control verification.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use erp_fields::{Cell, Decimal, ErrorKind, RecId, Value};
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID, UserId};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

async fn installed(db: &TestDb) -> (ModuleSystem, Orm) {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
    let native_artifact_identities = index
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
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system.initialize(InitOptions::new()).await.unwrap();
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    let env = system.system_env(erp_orm::Context::new());
    let installed = system
        .button_immediate_install(&env, &[ModuleName::new("uom").unwrap()])
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    (system, installed.orm)
}

async fn installed_policy(db: &TestDb, installed: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(installed.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

async fn user(tx: &mut erp_orm::Transaction, login: &str, company: i64, group: i64) -> i64 {
    let users = tx.env().records("res.users").unwrap();
    let user_record = users
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(login.into()))
                    .set_cell("login", Cell::Str(login.into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands(
                        "company_ids",
                        [Command::Set(vec![RecId::from_real(company).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::Set(vec![RecId::from_real(group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    user_record.ids()[0]
}

fn uom_vals(name: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("relative_factor", Cell::Decimal(Decimal::ONE))
}

async fn get_uom_views(
    orm: &Orm,
    types: &[&str],
    options: Option<erp_fields::ValueMap>,
) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("uom.uom").unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed on uom.uom")
    };
    let views = types
        .iter()
        .map(|t| Value::List(vec![Value::False, Value::str(*t)].into()))
        .collect::<Vec<_>>();
    let mut opt = options.unwrap_or_default();
    if !opt.contains_key("mobile") {
        opt.insert("mobile".into(), Value::Bool(false));
    }
    let args = erp_fields::ValueMap::from_iter([
        ("views".into(), Value::List(views.into())),
        ("wire_major".into(), Value::Int(1)),
        ("options".into(), Value::Map(Arc::new(opt))),
    ]);
    let value = method
        .call(&mut tx, records, RpcArgs::new(args))
        .await
        .unwrap_or_else(|e| panic!("uom.uom get_views: {e:?}"));
    tx.rollback().await.unwrap();
    erp_fields::convert::value_to_json(&value).unwrap()
}

fn field_names(view: &serde_json::Value) -> Vec<String> {
    view["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["field"]["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn spec_uom_017_ui_views_action_and_settings_menu_installed() {
    let db = TestDb::create().await.unwrap();
    let (_system, orm) = installed(&db).await;

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

    // 1. Verify XML IDs for views, actions, and menu in ir_model_data
    let view_ids: Vec<String> = tx
        .query(
            "SELECT module || '.' || name FROM ir_model_data \
             WHERE module = 'uom' AND model = 'ir.ui.view' ORDER BY name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        view_ids,
        [
            "uom.uom_uom_view_form",
            "uom.uom_uom_view_list",
            "uom.uom_uom_view_search"
        ]
    );

    // 2. Verify window action and view mappings
    let action_rows = tx
        .query(
            "SELECT a.name, a.res_model, a.view_mode, vl.name, vs.name \
             FROM ir_act_window a \
             JOIN ir_model_data da ON da.model = 'ir.actions.act_window' AND da.res_id = a.id \
             LEFT JOIN ir_ui_view vl ON vl.id = a.view_id \
             LEFT JOIN ir_ui_view vs ON vs.id = a.search_view_id \
             WHERE da.module = 'uom' AND da.name = 'uom_uom_action_list'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(action_rows.len(), 1, "window action exists");
    assert_eq!(action_rows[0].get::<_, String>(0), "Units & Packagings");
    assert_eq!(action_rows[0].get::<_, String>(1), "uom.uom");
    assert_eq!(action_rows[0].get::<_, String>(2), "list,form");
    assert_eq!(action_rows[0].get::<_, String>(3), "uom.uom.list");
    assert_eq!(action_rows[0].get::<_, String>(4), "uom.uom.view.search");

    let act_views: Vec<String> = tx
        .query(
            "SELECT v.view_mode || ':' || vd.name \
             FROM ir_act_window_view v \
             JOIN ir_model_data da ON da.model = 'ir.actions.act_window' AND da.res_id = v.act_window_id \
             JOIN ir_model_data vd ON vd.model = 'ir.ui.view' AND vd.res_id = v.view_id \
             WHERE da.module = 'uom' AND da.name = 'uom_uom_action_list' ORDER BY v.sequence",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        act_views,
        ["list:uom_uom_view_list", "form:uom_uom_view_form"]
    );

    // 3. Verify settings menu
    let menu_rows = tx
        .query(
            "SELECT m.name, parent.name, dg.name, a.res_model \
             FROM ir_ui_menu m \
             JOIN ir_model_data dm ON dm.model = 'ir.ui.menu' AND dm.res_id = m.id \
             JOIN ir_ui_menu parent ON parent.id = m.parent_id \
             JOIN ir_model_data dp ON dp.model = 'ir.ui.menu' AND dp.res_id = parent.id \
             JOIN ir_ui_menu_group_rel r ON r.menu_id = m.id \
             JOIN ir_model_data dg ON dg.model = 'res.groups' AND dg.res_id = r.gid \
             JOIN ir_model_data da ON da.model = 'ir.actions.act_window' AND m.action = 'ir.actions.act_window,' || da.res_id \
             JOIN ir_act_window a ON a.id = da.res_id \
             WHERE dm.module = 'uom' AND dm.name = 'menu_uom_form_action' \
               AND dp.module = 'base' AND dp.name = 'settings_menu' \
               AND dg.module = 'base' AND dg.name = 'group_system'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        menu_rows.len(),
        1,
        "menu sits under settings for system group"
    );
    assert_eq!(menu_rows[0].get::<_, String>(0), "Units of Measure");
    assert_eq!(menu_rows[0].get::<_, String>(3), "uom.uom");

    tx.rollback().await.unwrap();

    // 4. Verify get_views serves valid list, form, and search views for uom.uom
    let served = get_uom_views(&orm, &["list", "form", "search"], None).await;
    let views = served["views"].as_array().unwrap();
    assert_eq!(views.len(), 3, "served list, form, search: {served}");

    // List view fields: sequence, name, relative_factor, relative_uom_id
    let list_fields = field_names(&views[0]);
    assert_eq!(
        list_fields,
        ["sequence", "name", "relative_factor", "relative_uom_id"],
        "list view fields"
    );

    // Form view fields: name, relative_factor, relative_uom_id
    let form_fields = field_names(&views[1]);
    assert_eq!(
        form_fields,
        ["name", "relative_factor", "relative_uom_id"],
        "form view fields"
    );

    // Search view fields: name
    let search_fields = field_names(&views[2]);
    assert_eq!(search_fields, ["name"], "search view fields");

    db.cleanup().await.unwrap();
}

#[test]
fn spec_uom_016_complete_access_control_and_light_group() {
    // the installed-stack future is too large for the default test thread stack
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(uom_016_body());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn uom_016_body() {
    let db = TestDb::create().await.unwrap();
    let (_system, installed_orm) = installed(&db).await;

    let mut setup = installed_orm.begin(SUPERUSER_ID).await.unwrap();
    let company: i64 = setup
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let group_user: i64 = setup
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let group_system: i64 = setup
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);

    let manager_uid = user(&mut setup, "uom-admin", company, group_system).await;
    let reader_uid = user(&mut setup, "uom-user", company, group_user).await;
    setup.commit().await.unwrap();

    let orm = installed_policy(&db, &installed_orm).await;

    // 1. Standard user (base.group_user) is granted read
    let mut reader_tx = orm.begin(UserId::new(reader_uid).unwrap()).await.unwrap();
    let read_records = reader_tx
        .env()
        .records("uom.uom")
        .unwrap()
        .search(&mut reader_tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert!(!read_records.is_empty(), "standard user can read uom.uom");

    // Standard user is denied write/create/unlink
    let create_err = reader_tx
        .env()
        .records("uom.uom")
        .unwrap()
        .create(&mut reader_tx, vec![uom_vals("User Create")])
        .await
        .unwrap_err();
    assert_eq!(create_err.kind(), ErrorKind::Access);
    assert!(
        create_err
            .to_string()
            .contains("You are not allowed to create 'Unit of Measure' (uom.uom)"),
        "create denied: {create_err}"
    );

    let unit_to_edit = read_records.at(0).unwrap();
    let edit_vals = RawVals::new().set_cell("name", Cell::Str("Edited by user".into()));
    let write_err = unit_to_edit
        .write(&mut reader_tx, edit_vals)
        .await
        .unwrap_err();
    assert_eq!(write_err.kind(), ErrorKind::Access);
    assert!(
        write_err
            .to_string()
            .contains("You are not allowed to modify 'Unit of Measure' (uom.uom)"),
        "modify denied: {write_err}"
    );

    let unlink_err = unit_to_edit.unlink(&mut reader_tx).await.unwrap_err();
    assert_eq!(unlink_err.kind(), ErrorKind::Access);
    assert!(
        unlink_err
            .to_string()
            .contains("You are not allowed to delete 'Unit of Measure' (uom.uom)"),
        "delete denied: {unlink_err}"
    );
    reader_tx.rollback().await.unwrap();

    // 2. System manager (base.group_system) has full CRUD
    let mut mgr_tx = orm.begin(UserId::new(manager_uid).unwrap()).await.unwrap();
    let created = mgr_tx
        .env()
        .records("uom.uom")
        .unwrap()
        .create(&mut mgr_tx, vec![uom_vals("Manager Test UoM")])
        .await
        .unwrap();
    assert_eq!(created.len(), 1, "manager can create uom.uom");

    let update_vals = RawVals::new().set_cell("name", Cell::Str("Manager Test UoM Updated".into()));
    created.write(&mut mgr_tx, update_vals).await.unwrap();

    let read_back = created.cells(&mut mgr_tx, "name").await.unwrap();
    assert_eq!(
        read_back,
        vec![Cell::Str("Manager Test UoM Updated".into())]
    );

    created.unlink(&mut mgr_tx).await.unwrap();
    mgr_tx.commit().await.unwrap();

    // 3. Light group: uom.group_uom registered as light group without adding permissions
    let (_group_id, access_count, implied_empty) = {
        let client = db.client().await.unwrap();
        let rows = client
            .query(
                "SELECT res_id::bigint FROM ir_model_data WHERE module='uom' AND name='group_uom' AND model='res.groups'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "uom.group_uom must be installed");
        let group_id: i64 = rows[0].get(0);
        let access_count: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_access WHERE group_id=$1",
                &[&group_id],
            )
            .await
            .unwrap()
            .get(0);
        let implied = client
            .query(
                "SELECT hid FROM res_groups_implied_rel WHERE gid=$1",
                &[&group_id],
            )
            .await
            .unwrap();
        (group_id, access_count, implied.is_empty())
    };
    assert_eq!(
        access_count, 0,
        "light group must not grant model authority"
    );
    assert!(implied_empty, "uom.group_uom must not imply other groups");

    let mut super_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let groups = super_tx.env().records("res.groups").unwrap();
    let light = groups
        .call::<(), Vec<String>>(&mut super_tx, "_get_light_group_xmlids", ())
        .await
        .unwrap();
    assert!(
        light.iter().any(|xmlid| xmlid == "uom.group_uom"),
        "group_uom must be classified as a light group: {light:?}"
    );
    super_tx.rollback().await.unwrap();

    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn uom_upgrade_idempotence_preserves_views_and_records() {
    let db = TestDb::create().await.unwrap();
    let (system, _orm) = installed(&db).await;

    let client = db.client().await.unwrap();
    let count_before: i64 = client
        .query_one("SELECT count(*) FROM uom_uom", &[])
        .await
        .unwrap()
        .get(0);
    let views_before: i64 = client
        .query_one(
            "SELECT count(*) FROM ir_ui_view v JOIN ir_model_data d ON d.model='ir.ui.view' AND d.res_id=v.id WHERE d.module='uom'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    // Re-run system load / upgrade
    let loaded = system.load().await.unwrap();
    assert!(loaded.post_commit_pending.is_empty());
    assert!(loaded.unavailable.is_empty());

    let count_after: i64 = client
        .query_one("SELECT count(*) FROM uom_uom", &[])
        .await
        .unwrap()
        .get(0);
    let views_after: i64 = client
        .query_one(
            "SELECT count(*) FROM ir_ui_view v JOIN ir_model_data d ON d.model='ir.ui.view' AND d.res_id=v.id WHERE d.module='uom'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    assert_eq!(count_before, count_after, "unit count preserved on upgrade");
    assert_eq!(views_before, views_after, "view count preserved on upgrade");

    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn uom_installed_rpc_convert_and_precision_services() {
    let db = TestDb::create().await.unwrap();
    let (_system, orm) = installed(&db).await;

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

    // Look up Units and Dozen
    let unit_rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='uom' AND name='product_uom_unit'",
            &[],
        )
        .await
        .unwrap();
    let unit_id: i64 = unit_rows[0].get(0);

    let dozen_rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='uom' AND name='product_uom_dozen'",
            &[],
        )
        .await
        .unwrap();
    let dozen_id: i64 = dozen_rows[0].get(0);

    let units = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse(vec![unit_id])
        .unwrap();

    let Exposure::Exposed(convert_method) = tx
        .exposure()
        .get(units.model_id(), "convert")
        .unwrap()
        .clone()
    else {
        panic!("uom.uom.convert must be exposed");
    };

    // 1. Rounded convert: 24 units -> 2 dozens
    let args = erp_fields::ValueMap::from_iter([
        ("quantity".into(), Value::str("24")),
        ("to_unit".into(), Value::Int(dozen_id)),
        ("exact".into(), Value::Bool(false)),
        ("rounding_method".into(), Value::str("UP")),
    ]);
    let result = convert_method
        .call(&mut tx, units.clone(), RpcArgs::new(args))
        .await
        .unwrap();

    let json = erp_fields::convert::value_to_json(&result).unwrap();
    assert_eq!(json["value"].as_str(), Some("2.00"));
    assert_eq!(json["rounding"].as_str(), Some("UP"));
    assert_eq!(json["digits"].as_i64(), Some(2));

    // 2. Exact convert: 1 unit -> 1/12 dozen
    let args_exact = erp_fields::ValueMap::from_iter([
        ("quantity".into(), Value::str("1")),
        ("to_unit".into(), Value::Int(dozen_id)),
        ("exact".into(), Value::Bool(true)),
    ]);
    let result_exact = convert_method
        .call(&mut tx, units, RpcArgs::new(args_exact))
        .await
        .unwrap();

    let json_exact = erp_fields::convert::value_to_json(&result_exact).unwrap();
    assert_eq!(json_exact["numerator"].as_str(), Some("1"));
    assert_eq!(json_exact["denominator"].as_str(), Some("12"));

    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}
