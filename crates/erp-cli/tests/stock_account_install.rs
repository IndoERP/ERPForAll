// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real valuation-addon installation, default data boundary and repeat upgrade.
//! This is current-source lifecycle coverage; a genuine previous-native-binary upgrade is unverified.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_sdk::{
    Cell, Command, Context, ErrorKind, Operation, Orm, RawVals, RecId, SUPERUSER_ID, UserId,
};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use std::path::Path;
use std::sync::Arc;

const MODELS: [&str; 10] = [
    "stock.valuation.source",
    "stock.valuation.source.alloc",
    "stock.valuation.receipt",
    "stock.valuation.journal",
    "stock.valuation.remainder",
    "stock.valuation.state",
    "stock.valuation.control",
    "stock.valuation.checkpoint",
    "stock.valuation.layer",
    "stock.valuation.layer.consumption",
];

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
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
    ModuleSystem::builder(db.pool().clone(), index)
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_barcodes::ADDON,
            &erp_addon_barcodes_gs1_nomenclature::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_stock::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_stock_account::ADDON,
        ])
        .build()
        .unwrap()
}

#[test]
fn valuation_install_and_repeat_upgrade_keep_stock_only_boundary_and_deny_default() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(install());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn install() {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    assert!(init.post_commit.is_clean());
    let stock = Box::pin(system.button_immediate_install(
        &init.orm.env(SUPERUSER_ID, Context::new()),
        &[ModuleName::new("stock").unwrap()],
    ))
    .await
    .unwrap();
    assert!(stock.post_commit.is_clean());
    assert!(!stock.orm.registry().contains("stock.valuation.source"));
    let mut tx = stock.orm.begin(SUPERUSER_ID).await.unwrap();
    assert!(tx.query("SELECT tablename FROM pg_tables WHERE schemaname=current_schema() AND tablename LIKE 'stk_%'",&[]).await.unwrap().is_empty());
    assert_eq!(
        tx.query(
            "SELECT state::text FROM ir_module_module WHERE name='account'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, String>(0),
        "uninstalled"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let installed = Box::pin(system.button_immediate_install(
        &stock.orm.env(SUPERUSER_ID, Context::new()),
        &[ModuleName::new("stock_account").unwrap()],
    ))
    .await
    .unwrap();
    assert!(installed.post_commit.is_clean());
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    for name in ["stock_account", "account", "analytic", "stock"] {
        assert_eq!(
            tx.query(
                "SELECT state::text FROM ir_module_module WHERE name=$1",
                &[&name]
            )
            .await
            .unwrap()[0]
                .get::<_, String>(0),
            "installed"
        );
    }
    for model in MODELS {
        let spec = installed.orm.registry().model(model).unwrap();
        assert_eq!(
            tx.query(&format!("SELECT count(*)::int8 FROM {}", spec.table), &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
    }
    // No CRUD permission is granted; generated company restrictions are not access grants.
    assert_eq!(tx.query("SELECT count(*)::int8 FROM ir_access a JOIN ir_model m ON m.id=a.model_id WHERE m.model LIKE 'stock.valuation.%' AND NOT coalesce(a.generated,false)",&[]).await.unwrap()[0].get::<_,i64>(0),0);
    let company: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let admin_group: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let users = tx.env().records("res.users").unwrap();
    let admin = users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Valuation Admin".into()))
                    .set_cell("login", Cell::Str("valuation_admin".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands(
                        "company_ids",
                        [Command::Set(vec![RecId::from_real(company).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::Set(vec![RecId::from_real(admin_group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    let uid = UserId::new(admin.rec_ids()[0].get()).unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // ModuleSystem's operation ORM is SuperuserOnly. Exercise actual installed ACLs through
    // a resolved user and the compiled database policy, as the runtime does.
    let policy = PolicySource::build(Arc::clone(installed.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let runtime = Orm::builder(Arc::clone(installed.orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(installed.orm.registry()).unwrap())
        .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut denied = runtime.begin_caller(uid).await.unwrap();
    for model in MODELS {
        let records = denied.env().records(model).unwrap();
        for (operation, verb) in [
            (Operation::Read, "access"),
            (Operation::Create, "create"),
            (Operation::Write, "modify"),
            (Operation::Unlink, "delete"),
        ] {
            let error = records
                .check_access(&mut denied, operation)
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::Access,
                "{model}/{operation:?}: {error:?}"
            );
            let description = &installed.orm.registry().model(model).unwrap().description;
            assert_eq!(
                error.message(),
                format!(
                    "Your access rights do not let you {verb} '{description}' ({model}) records.\n\nNo group may do this.\n\nAsk an administrator if you need this access."
                )
            );
        }
    }
    denied.rollback().await.unwrap();
    drop(denied);
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let source = tx.env().records("stock.valuation.source").unwrap();
    let defaults = source
        .default_get(&mut tx, &["component", "cost_partition"])
        .await
        .unwrap();
    assert_eq!(
        defaults.get("component"),
        Some(&erp_sdk::Value::Str("".into()))
    );
    assert_eq!(
        defaults.get("cost_partition"),
        Some(&erp_sdk::Value::Str("".into()))
    );
    let c: i64 = tx
        .query("SELECT id FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let tmpl:i64=tx.query("INSERT INTO product_template(name,uom_id,type) SELECT '{\"en_US\":\"Valuation fixture\"}',id,'consu' FROM uom_uom ORDER BY id LIMIT 1 RETURNING id",&[]).await.unwrap()[0].get(0);
    let product: i64 = tx
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&tmpl],
        )
        .await
        .unwrap()[0]
        .get(0);
    let records = tx.env().records("stock.valuation.control").unwrap();
    let created = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("company_id", Cell::Id(c))
                    .set_cell("product_id", Cell::Id(product)),
            ],
        )
        .await
        .unwrap();
    let id = created.rec_ids()[0].get();
    tx.commit().await.unwrap();
    drop(tx);
    let upgraded = Box::pin(system.button_immediate_upgrade(
        &installed.orm.env(SUPERUSER_ID, Context::new()),
        &[ModuleName::new("stock_account").unwrap()],
    ))
    .await
    .unwrap();
    assert!(upgraded.post_commit.is_clean());
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT worker_epoch FROM stk_cost_control WHERE id=$1",
            &[&id]
        )
        .await
        .unwrap()[0]
            .get::<_, i32>(0),
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}
