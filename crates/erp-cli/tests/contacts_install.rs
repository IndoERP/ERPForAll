//! The data-only `contacts` addon: installed on top of base, its records, views, menu and
//! actions resolve, the view arches validate on get_views, and the demo contacts load with demo data.
//! The addon defines no model of its own, so it has no default records beyond the demo partners and tags.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use erp_fields::{Cell, RecId, Value};
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID};
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

fn system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_web::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

/// Runs `body` on a thread with a large stack: a full install chain exceeds the default one.
fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// A database with base and web initialised, then `contacts` installed.
async fn with_contacts(demo: bool) -> (TestDb, Orm) {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let _init = system
        .initialize(InitOptions::new().with_demo(demo))
        .await
        .unwrap();
    let env = system.system_env(erp_orm::Context::new());
    let installed = system
        .button_immediate_install(&env, &[ModuleName::new("contacts").unwrap()])
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    (db, installed.orm)
}

async fn strings(orm: &Orm, sql: &str) -> Vec<String> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

async fn get_views(orm: &Orm, model: &str, types: &[&str]) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records(model).unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed on {model}")
    };
    let views = types
        .iter()
        .map(|t| Value::List(vec![Value::False, Value::str(*t)].into()))
        .collect::<Vec<_>>();
    let args = erp_fields::ValueMap::from_iter([
        ("views".into(), Value::List(views.into())),
        ("wire_major".into(), Value::Int(1)),
        (
            "options".into(),
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                "mobile".into(),
                Value::Bool(false),
            )]))),
        ),
    ]);
    let value = method
        .call(&mut tx, records, RpcArgs::new(args))
        .await
        .unwrap_or_else(|e| panic!("{model} get_views: {e:?}"));
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

/// External ids (`module.name`) of one model, sorted.
async fn xml_ids(orm: &Orm, module: &str, model: &str) -> Vec<String> {
    strings(
        orm,
        &format!(
            "SELECT module || '.' || name FROM ir_model_data \
             WHERE module = '{module}' AND model = '{model}' ORDER BY name"
        ),
    )
    .await
}

/// Every new record of the addon and the partner views base now ships, by external id.
#[test]
fn contacts_install_creates_every_record_by_xml_id() {
    big_stack(async {
        let (_db, orm) = with_contacts(false).await;
        assert_eq!(
            xml_ids(&orm, "contacts", "ir.actions.act_window").await,
            ["contacts.action_contact_tags", "contacts.action_contacts"]
        );
        assert_eq!(
            xml_ids(&orm, "contacts", "ir.actions.act_window.view").await,
            [
                "contacts.action_contact_tags_form",
                "contacts.action_contact_tags_list",
                "contacts.action_contacts_form",
                "contacts.action_contacts_list",
            ]
        );
        assert_eq!(
            xml_ids(&orm, "contacts", "ir.ui.menu").await,
            [
                "contacts.menu_contact_tags",
                "contacts.menu_contacts",
                "contacts.menu_contacts_config",
                "contacts.menu_contacts_root",
                "contacts.menu_country_group",
                "contacts.menu_country_partner",
                "contacts.menu_country_state_partner",
                "contacts.menu_localisation",
            ]
        );
        let base_views = xml_ids(&orm, "base", "ir.ui.view").await;
        for id in [
            "base.partner_list",
            "base.partner_form",
            "base.partner_search",
            "base.partner_category_list",
            "base.partner_category_form",
        ] {
            assert!(
                base_views.contains(&id.to_owned()),
                "{id} in {base_views:?}"
            );
        }
        // No demo data without demo: the addon ships no default records of its own.
        assert!(xml_ids(&orm, "contacts", "res.partner").await.is_empty());
    });
}

/// The menu resolves to the action, and the action opens list then form over res.partner.
#[test]
fn contacts_menu_resolves_to_a_list_and_form_action() {
    big_stack(async {
        let (_db, orm) = with_contacts(false).await;
        // Top-level menu: opens the Contacts action, visible to internal users (base.group_user).
        assert_eq!(
            strings(
                &orm,
                "SELECT a.res_model || '|' || a.view_mode || '|' || d.module || '.' || d.name || '|' || dg.name \
                 FROM ir_ui_menu m \
                 JOIN ir_model_data dm ON dm.model = 'ir.ui.menu' AND dm.res_id = m.id \
                 JOIN ir_model_data d ON d.model = 'ir.actions.act_window' \
                  AND m.action = 'ir.actions.act_window,' || d.res_id \
                 JOIN ir_act_window a ON a.id = d.res_id \
                 JOIN ir_ui_menu_group_rel r ON r.menu_id = m.id \
                 JOIN ir_model_data dg ON dg.model = 'res.groups' AND dg.res_id = r.gid \
                 WHERE dm.module = 'contacts' AND dm.name = 'menu_contacts_root' \
                   AND m.parent_id IS NULL AND m.web_icon = 'lucide:contact'"
            )
            .await,
            ["res.partner|list,form|contacts.action_contacts|group_user"]
        );
        // The Contact Tags entry sits under Configuration (administrators) and opens the tag action.
        assert_eq!(
            strings(
                &orm,
                "SELECT a.res_model || '|' || dp.name || '|' || dg.name \
                 FROM ir_ui_menu m \
                 JOIN ir_model_data dm ON dm.model = 'ir.ui.menu' AND dm.res_id = m.id \
                 JOIN ir_model_data dp ON dp.model = 'ir.ui.menu' AND dp.res_id = m.parent_id \
                 JOIN ir_model_data da ON da.model = 'ir.actions.act_window' \
                  AND m.action = 'ir.actions.act_window,' || da.res_id \
                 JOIN ir_act_window a ON a.id = da.res_id \
                 JOIN ir_ui_menu cfg ON cfg.id = m.parent_id \
                 JOIN ir_ui_menu_group_rel r ON r.menu_id = cfg.id \
                 JOIN ir_model_data dg ON dg.model = 'res.groups' AND dg.res_id = r.gid \
                 WHERE dm.module = 'contacts' AND dm.name = 'menu_contact_tags'"
            )
            .await,
            ["res.partner.category|menu_contacts_config|group_system"]
        );
        // The action's per-type views, in order: list then form.
        assert_eq!(
            strings(
                &orm,
                "SELECT v.view_mode || ':' || vd.name \
                 FROM ir_act_window_view v \
                 JOIN ir_model_data da ON da.model = 'ir.actions.act_window' AND da.res_id = v.act_window_id \
                 JOIN ir_model_data vd ON vd.model = 'ir.ui.view' AND vd.res_id = v.view_id \
                 WHERE da.module = 'contacts' AND da.name = 'action_contacts' ORDER BY v.sequence"
            )
            .await,
            ["list:partner_list", "form:partner_form"]
        );
    });
}

/// View arches validate when retrieved via get_views (not during addon install) and serve the documented fields.
#[test]
fn partner_views_validate_and_serve_the_documented_fields() {
    big_stack(async {
        let (_db, orm) = with_contacts(false).await;
        let served = get_views(&orm, "res.partner", &["list", "form", "search"]).await;
        let views = served["views"].as_array().unwrap();
        assert_eq!(views.len(), 3, "{served}");
        assert_eq!(
            field_names(&views[0]),
            [
                "complete_name",
                "phone",
                "email",
                "city",
                "vat",
                "category_id"
            ]
        );
        let form = field_names(&views[1]);
        for field in [
            "name",
            "is_company",
            "parent_id",
            "type",
            "vat",
            "ref",
            "email",
            "phone",
            "website",
            "child_ids",
            "street",
            "street2",
            "zip",
            "city",
            "industry_id",
            "category_id",
            "company_id",
            "active",
        ] {
            assert!(
                form.contains(&field.to_owned()),
                "form lacks {field}: {form:?}"
            );
        }
        // parent_id carries invisible="is_company" modifier in form view
        let parent_id_node = views[1]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["field"]["name"] == "parent_id")
            .expect("form view has parent_id field");
        assert_eq!(parent_id_node["modifiers"]["invisible"]["schema"], 1);
        assert_eq!(
            parent_id_node["modifiers"]["invisible"]["nodes"][0]["name"],
            "is_company"
        );

        let search = field_names(&views[2]);
        for field in [
            "complete_name",
            "email",
            "phone",
            "parent_id",
            "category_id",
        ] {
            assert!(
                search.contains(&field.to_owned()),
                "search lacks {field}: {search:?}"
            );
        }
        // The search filters: companies, individuals, archived
        let filter_names: Vec<&str> = views[2]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "search_filter")
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(
            filter_names.contains(&"companies"),
            "search lacks 'companies' filter: {filter_names:?}"
        );
        assert!(
            filter_names.contains(&"individuals"),
            "search lacks 'individuals' filter: {filter_names:?}"
        );
        assert!(
            filter_names.contains(&"archived"),
            "search lacks 'archived' filter: {filter_names:?}"
        );

        // The group-by items a person can try in the list's Group By menu: each carries its grouping
        // field in the filter's context expression.
        let group_filters: Vec<&str> = views[2]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "search_filter" && n["context"].is_object())
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert_eq!(
            group_filters,
            ["group_city", "group_type"],
            "search group-by filters: {group_filters:?}"
        );
        // Each context is `{'group_by': <field>}`: its string literals are the key, then the field.
        let literals = |name: &str| -> Vec<String> {
            views[2]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["kind"] == "search_filter" && n["name"] == name)
                .unwrap()["context"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|n| n["value"]["value"].as_str().map(str::to_owned))
                .collect()
        };
        assert_eq!(literals("group_city"), ["group_by", "city"]);
        assert_eq!(literals("group_type"), ["group_by", "type"]);

        let tags = get_views(&orm, "res.partner.category", &["list", "form"]).await;
        let tag_views = tags["views"].as_array().unwrap();
        assert_eq!(field_names(&tag_views[0]), ["name", "parent_id"]);
        let tag_form = field_names(&tag_views[1]);
        for field in ["name", "parent_id", "color", "active"] {
            assert!(
                tag_form.contains(&field.to_owned()),
                "tag form lacks {field}"
            );
        }
    });
}

/// Demo data: two companies (root partners with a tax ID) with child contacts, and two tags.
#[test]
fn contacts_demo_partners_load_with_demo() {
    big_stack(async {
        let (_db, orm) = with_contacts(true).await;
        assert_eq!(
            strings(
                &orm,
                "SELECT p.name || ':' || p.is_company::text || ':' || \
                 (SELECT count(*) FROM res_partner c WHERE c.parent_id = p.id)::text \
                 FROM res_partner p JOIN ir_model_data d \
                  ON d.model = 'res.partner' AND d.res_id = p.id \
                 WHERE d.module = 'contacts' AND d.name IN ('partner_demo_lumbung', 'partner_demo_tirta') \
                 ORDER BY p.name"
            )
            .await,
            ["Lumbung Teknik:true:2", "Tirta Logistik:true:1"]
        );
        assert_eq!(
            xml_ids(&orm, "contacts", "res.partner.category")
                .await
                .len(),
            2
        );
    });
}

/// An internal user (base.group_user) can read contact tags and industries, but is refused
/// when attempting to create a partner (which requires base.group_system).
#[test]
fn internal_user_reads_tags_and_industries_and_is_refused_on_partner_create() {
    big_stack(async {
        let (db, orm) = with_contacts(false).await;
        let mut setup_tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let admin_group_rows = setup_tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'group_system' AND model = 'res.groups'",
                &[],
            )
            .await
            .unwrap();
        let admin_group: i64 = admin_group_rows[0].get(0);
        let user_group_rows = setup_tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'group_user' AND model = 'res.groups'",
                &[],
            )
            .await
            .unwrap();
        let user_group: i64 = user_group_rows[0].get(0);
        let company_rows = setup_tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_company' AND model = 'res.company'",
                &[],
            )
            .await
            .unwrap();
        let main_company: i64 = company_rows[0].get(0);
        let users = setup_tx.env().records("res.users").unwrap();
        // An active administrator user must exist for the database invariant to hold.
        users
            .create(
                &mut setup_tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Test Admin".into()))
                        .set_cell("login", Cell::Str("test_admin".into()))
                        .set_cell("company_id", Cell::Id(main_company))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main_company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(admin_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let internal_user = users
            .create(
                &mut setup_tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Internal Reader".into()))
                        .set_cell("login", Cell::Str("internal_reader".into()))
                        .set_cell("company_id", Cell::Id(main_company))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main_company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(user_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user_id = internal_user.ids()[0];
        setup_tx.commit().await.unwrap();

        let source = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
        let runtime_orm = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
            .require_core_tables(erp_security::auth::core_tables())
            .resolver(Resolver::for_registry(orm.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();

        let mut user_tx = runtime_orm
            .begin(erp_orm::UserId::new(user_id).unwrap())
            .await
            .unwrap();
        // 1. Can read contact tags (res.partner.category)
        let tags = user_tx.env().records("res.partner.category").unwrap();
        let found_tags = tags
            .search(&mut user_tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap();
        let _ = found_tags.read(&mut user_tx, &["name"]).await.unwrap();

        // 2. Can read industries (res.partner.industry)
        let industries = user_tx.env().records("res.partner.industry").unwrap();
        let found_industries = industries
            .search(&mut user_tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap();
        let _ = found_industries
            .read(&mut user_tx, &["name"])
            .await
            .unwrap();

        // 3. Refused on partner create (res.partner)
        let partners = user_tx.env().records("res.partner").unwrap();
        let err = partners
            .create(
                &mut user_tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Denied Partner".into()))],
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
        assert!(
            err.message().starts_with(
                "Your access rights do not let you create 'Contact' (res.partner) records."
            ),
            "unexpected error message: {}",
            err.message()
        );
        assert!(
            err.message().contains("Settings Administration"),
            "unexpected error message: {}",
            err.message()
        );
        user_tx.rollback().await.unwrap();
    });
}

/// A database initialized at base 1.6 picks up `partner_views.xml` and the four access rows
/// for contact tags and industries upon upgrading base.
#[test]
fn base_upgrade_from_1_6_picks_up_partner_views_and_access_rows() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let sys = system(&db);
        let _init = sys
            .initialize(InitOptions::new().with_demo(false))
            .await
            .unwrap();
        let client = db.client().await.unwrap();

        let view_xml_ids: &[&str] = &[
            "partner_list",
            "partner_form",
            "partner_search",
            "partner_category_list",
            "partner_category_form",
        ];
        let access_xml_ids: &[&str] = &[
            "access_partner_category_system",
            "access_partner_category_internal",
            "access_partner_industry_system",
            "access_partner_industry_internal",
        ];

        // 1. Delete views and their xml_ids to simulate base 1.6
        client
            .execute(
                "DELETE FROM ir_ui_view WHERE id IN (\
                    SELECT res_id FROM ir_model_data \
                    WHERE module = 'base' AND model = 'ir.ui.view' AND name = ANY($1)\
                 )",
                &[&view_xml_ids],
            )
            .await
            .unwrap();
        client
            .execute(
                "DELETE FROM ir_model_data WHERE module = 'base' AND model = 'ir.ui.view' AND name = ANY($1)",
                &[&view_xml_ids],
            )
            .await
            .unwrap();

        // Delete the four access rows and their xml_ids
        client
            .execute(
                "DELETE FROM ir_access WHERE id IN (\
                    SELECT res_id FROM ir_model_data \
                    WHERE module = 'base' AND model = 'ir.access' AND name = ANY($1)\
                 )",
                &[&access_xml_ids],
            )
            .await
            .unwrap();
        client
            .execute(
                "DELETE FROM ir_model_data WHERE module = 'base' AND model = 'ir.access' AND name = ANY($1)",
                &[&access_xml_ids],
            )
            .await
            .unwrap();

        // Reset base latest_version to 1.6.0
        client
            .execute(
                "UPDATE ir_module_module SET latest_version = '1.6.0' WHERE name = 'base'",
                &[],
            )
            .await
            .unwrap();

        // Verify none of them exist before upgrade
        let count_views: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_model_data WHERE module = 'base' AND model = 'ir.ui.view' AND name = ANY($1)",
                &[&view_xml_ids],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count_views, 0);

        let count_access: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_model_data WHERE module = 'base' AND model = 'ir.access' AND name = ANY($1)",
                &[&access_xml_ids],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count_access, 0);

        // 2. Perform immediate upgrade of base
        let env = sys.system_env(erp_orm::Context::new());
        let upgraded = sys
            .button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()])
            .await
            .unwrap();
        assert!(
            upgraded.post_commit.is_clean(),
            "{:?}",
            upgraded.post_commit
        );

        // 3. Verify all 5 views and 4 access rows are picked up
        let base_views = xml_ids(&upgraded.orm, "base", "ir.ui.view").await;
        for id in [
            "base.partner_list",
            "base.partner_form",
            "base.partner_search",
            "base.partner_category_list",
            "base.partner_category_form",
        ] {
            assert!(
                base_views.contains(&id.to_owned()),
                "Upgraded base lacks view {id}: {base_views:?}"
            );
        }

        let base_access = xml_ids(&upgraded.orm, "base", "ir.access").await;
        for id in [
            "base.access_partner_category_system",
            "base.access_partner_category_internal",
            "base.access_partner_industry_system",
            "base.access_partner_industry_internal",
        ] {
            assert!(
                base_access.contains(&id.to_owned()),
                "Upgraded base lacks access row {id}: {base_access:?}"
            );
        }

        // 4. Verify views can be served
        let served = get_views(&upgraded.orm, "res.partner", &["list", "form", "search"]).await;
        assert_eq!(served["views"].as_array().unwrap().len(), 3);
    });
}

#[test]
fn mail_install_on_contacts_serves_chatter_on_partner_form() {
    big_stack(async {
        let (db, orm) = with_contacts(false).await;
        // Verify partner form before mail has no chatter node
        let before = get_views(&orm, "res.partner", &["form"]).await;
        let views_before = before["views"].as_array().unwrap();
        assert_eq!(views_before.len(), 1);
        let form_before = &views_before[0];
        assert!(
            !form_before["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["kind"] == "chatter"),
            "form before mail must not carry chatter node"
        );

        // Install mail on the same database
        let system = system(&db);
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(&env, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let orm_after = outcome.orm;

        // Verify partner form served to client carries the chatter node
        let after = get_views(&orm_after, "res.partner", &["form"]).await;
        let views_after = after["views"].as_array().unwrap();
        assert_eq!(views_after.len(), 1);
        let form_after = &views_after[0];
        let chatter = form_after["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["kind"] == "chatter")
            .expect("partner form arch served to client must carry chatter node");
        assert_eq!(chatter["kind"], "chatter");
        assert_eq!(chatter["reload_on_post"], false);
        assert_eq!(chatter["reload_on_attachment"], false);
        assert_eq!(chatter["reload_on_activity"], false);
        assert_eq!(chatter["reload_on_follower"], false);
        assert_eq!(chatter["open_attachments"], false);
        assert_eq!(chatter["has_activities"], false);

        // Confirm res.partner records inherit mail.thread
        let mut tx = orm_after.begin(SUPERUSER_ID).await.unwrap();
        let partner_records = tx.env().records("res.partner").unwrap();
        assert!(
            partner_records.spec().inherits("mail.thread"),
            "res.partner must inherit mail.thread after mail install"
        );
        tx.rollback().await.unwrap();
    });
}
