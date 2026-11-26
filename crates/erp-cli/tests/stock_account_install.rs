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
    Cell, Command, Context, Date, ErrorKind, Operation, Orm, RawVals, RecId, SUPERUSER_ID, UserId,
};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use std::path::Path;
use std::sync::Arc;

#[path = "stock_account_install/cost_policy.rs"]
mod cost_policy;
#[path = "support/report_frame.rs"]
mod report_frame;

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
        .require_sql_objects([
            &erp_addon_account::reconcile::history_guard::MANIFEST,
            &erp_addon_stock::schema_guards::MANIFEST,
            &erp_addon_stock_account::history_guards::MANIFEST,
        ])
        .require_sql_uninstall_guards([&erp_addon_stock_account::uninstall_guard::MANIFEST])
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
            &erp_addon_reporting::ADDON,
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
    let stock_group = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='group_stock_user'", &[]).await.unwrap()[0].get::<_,i64>(0);
    let accounting_group: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='account' AND name='group_account_manager'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let stock_manager_group = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='stock' AND name='group_stock_manager'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let users = tx.env().records("res.users").unwrap();
    let create_user = |name: &str, group: i64| {
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("login", Cell::Str(name.into()))
            .set_cell("company_id", Cell::Id(company))
            .set_commands(
                "company_ids",
                [Command::Set(vec![RecId::from_real(company).unwrap()])],
            )
            .set_commands(
                "group_ids",
                [Command::Set(vec![RecId::from_real(group).unwrap()])],
            )
    };
    let created = users
        .create(
            &mut tx,
            vec![
                create_user("valuation_admin", admin_group),
                create_user("inventory_reader", stock_group),
                create_user("valuation_book_admin", accounting_group),
                create_user("valuation_book_reader", stock_manager_group),
            ],
        )
        .await
        .unwrap();
    let uid = UserId::new(created.rec_ids()[0].get()).unwrap();
    let stock_user = UserId::new(created.rec_ids()[1].get()).unwrap();
    let book_admin = UserId::new(created.rec_ids()[2].get()).unwrap();
    let book_reader = UserId::new(created.rec_ids()[3].get()).unwrap();
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
    let book = verify_management_book(
        &installed.orm,
        &runtime,
        company,
        book_admin,
        book_reader,
        stock_user,
    )
    .await;
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
    let tmpl:i64=tx.query("INSERT INTO product_template(name,uom_id,type,tracking) SELECT '{\"en_US\":\"Valuation fixture\"}',id,'consu','none' FROM uom_uom ORDER BY id LIMIT 1 RETURNING id",&[]).await.unwrap()[0].get(0);
    let product: i64 = tx
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&tmpl],
        )
        .await
        .unwrap()[0]
        .get(0);
    let records = tx.env().records("stock.valuation.control").unwrap();
    let error = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("company_id", Cell::Id(c))
                    .set_cell("product_id", Cell::Id(product)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    let id: i64 = tx.query("INSERT INTO stk_cost_control(company_id,product_id,worker_epoch,seal_revision,sealed,publication_generation,recorded_at) VALUES($1,$2,0,0,false,0,'2026-01-01') RETURNING id", &[&c,&product]).await.unwrap()[0].get(0);
    tx.commit().await.unwrap();
    drop(tx);
    verify_valuation_report(
        &installed.orm,
        &runtime,
        company,
        product,
        book_reader,
        stock_user,
    )
    .await;
    let old_report = rewind_stock_report(&installed.orm).await;
    let stock_addon = ModuleName::new("stock").unwrap();
    let accounting_addon = ModuleName::new("stock_account").unwrap();
    let upgraded = Box::pin(system.button_immediate_upgrade(
        &installed.orm.env(SUPERUSER_ID, Context::new()),
        &[stock_addon.clone(), accounting_addon],
    ))
    .await
    .unwrap();
    assert!(upgraded.post_commit.is_clean());
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    let built_in_target = stock_report_route(&mut tx).await;
    assert_ne!(built_in_target, old_report);
    let report = tx.env().records("reporting.report").unwrap();
    assert_eq!(
        report
            .browse([old_report])
            .unwrap()
            .cell(&mut tx, "name")
            .await
            .unwrap(),
        Cell::Str("Customized legacy stock report".into())
    );
    assert_eq!(
        report
            .browse([built_in_target])
            .unwrap()
            .cell(&mut tx, "name")
            .await
            .unwrap(),
        Cell::Str("Stock Report".into())
    );
    assert_eq!(
        tx.query(
            "SELECT a.params->>'custom' FROM ir_act_client a JOIN ir_model_data x ON x.res_id=a.id
             WHERE x.module='stock' AND x.name='action_report_stock_position'",
            &[],
        )
        .await
        .unwrap()[0]
            .get::<_, String>(0),
        "preserve me"
    );
    let configured_book = tx
        .env()
        .records("stock.cost.book")
        .unwrap()
        .browse([book])
        .unwrap();
    assert_eq!(
        configured_book.cell(&mut tx, "method").await.unwrap(),
        Cell::Str("lifo".into())
    );
    assert_eq!(
        configured_book.cell(&mut tx, "purpose").await.unwrap(),
        Cell::Str("management".into())
    );
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
    // Reapply the cutover to a copy of the older version marker with a customized target.
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    let custom_target: i64 = tx.query(
        "SELECT res_id FROM ir_model_data WHERE module='stock' AND name='report_moves_analysis'",
        &[],
    ).await.unwrap()[0].get(0);
    set_stock_report_route(&mut tx, custom_target).await;
    tx.commit().await.unwrap();
    drop(tx);
    let upgraded = Box::pin(system.button_immediate_upgrade(
        &upgraded.orm.env(SUPERUSER_ID, Context::new()),
        &[stock_addon],
    ))
    .await
    .unwrap();
    assert!(upgraded.post_commit.is_clean());
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(stock_report_route(&mut tx).await, custom_target);
    tx.rollback().await.unwrap();
    drop(tx);

    // The control row has no protected source or journal. Empty-history uninstall must work.
    let module = [ModuleName::new("stock_account").unwrap()];
    let caller = system.system_env(Context::new());
    let emptied = Box::pin(system.button_immediate_uninstall(&caller, &module))
        .await
        .unwrap();
    assert!(emptied.uninstalled.contains(&module[0]));
    let installed = Box::pin(system.button_immediate_install(&caller, &module))
        .await
        .unwrap();
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) VALUES($1,$2,'product.product',$2,'opening',0,'Movement','','2026-01-01','2026-01-01','average','cost_scale_v1',1,1,'{}','fixture')", &[&c,&product]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let error = Box::pin(system.button_immediate_uninstall(&caller, &module))
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("unexpected uninstall failure: {error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Stock valuation history prevents uninstalling Stock Account. (1)"
    );
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        tx.query("SELECT count(*)::int8 FROM stk_value_source", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        tx.query(
            "SELECT state::text FROM ir_module_module WHERE name='stock_account'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, String>(0),
        "installed"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    verify_receipt_preview(&installed.orm, &db, company, uid, stock_user).await;
    db.cleanup().await.unwrap();
}

async fn stock_report_route(tx: &mut erp_sdk::Transaction) -> i64 {
    tx.query(
        "SELECT (a.params->>'report')::int8 FROM ir_act_client a
         JOIN ir_model_data x ON x.res_id=a.id AND x.model='ir.actions.client'
         WHERE x.module='stock' AND x.name='action_report_stock_position'",
        &[],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn set_stock_report_route(tx: &mut erp_sdk::Transaction, target: i64) {
    tx.execute(
        "UPDATE ir_act_client a
         SET params=jsonb_build_object('report', $1::int8, 'custom', 'preserve me')
         FROM ir_model_data x WHERE x.res_id=a.id AND x.model='ir.actions.client'
           AND x.module='stock' AND x.name='action_report_stock_position'",
        &[&target],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE ir_model_data SET noupdate=false
         WHERE module='stock' AND name='action_report_stock_position'",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE ir_module_module SET latest_version='1.11.0' WHERE name='stock'",
        &[],
    )
    .await
    .unwrap();
}

async fn rewind_stock_report(orm: &Orm) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let legacy: i64 = tx.query(
        "SELECT res_id FROM ir_model_data WHERE module='stock' AND name='report_stock_position'",
        &[],
    ).await.unwrap()[0].get(0);
    let quantity = stock_report_route(&mut tx).await;
    set_stock_report_route(&mut tx, legacy).await;
    tx.execute("DELETE FROM reporting_report WHERE id=$1", &[&quantity])
        .await
        .unwrap();
    tx.execute(
        "DELETE FROM ir_model_data
         WHERE module='stock' AND starts_with(name, 'report_stock_quantity')",
        &[],
    )
    .await
    .unwrap();
    for (name, model) in [
        ("menu_report_stock_valuation", "ir.ui.menu"),
        ("action_report_stock_valuation", "ir.actions.client"),
    ] {
        let id: i64 = tx.query(
            "DELETE FROM ir_model_data WHERE module='stock_account' AND name=$1 RETURNING res_id",
            &[&name],
        ).await.unwrap()[0].get(0);
        tx.env()
            .records(model)
            .unwrap()
            .browse([id])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap();
    }
    tx.execute(
        "UPDATE ir_module_module SET latest_version='1.6.0' WHERE name='stock_account'",
        &[],
    )
    .await
    .unwrap();
    tx.env()
        .records("reporting.report")
        .unwrap()
        .browse([legacy])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Customized legacy stock report".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    legacy
}

#[test]
fn management_cost_book_policy_is_immutable_through_base_writer() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(verify_base_writer_cost_book_policy());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn verify_base_writer_cost_book_policy() {
    let db = TestDb::create().await.unwrap();
    let lifecycle = system(&db);
    let initialized = lifecycle
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = lifecycle
        .button_immediate_install(
            &lifecycle.system_env(Context::new()),
            &[ModuleName::new("stock_account").unwrap()],
        )
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    let orm = installed.orm;

    let mut root = orm.begin(SUPERUSER_ID).await.unwrap();
    let data = root.query(
        "SELECT c.id, c.currency_id,
                (SELECT id FROM res_currency r WHERE r.id <> c.currency_id ORDER BY id LIMIT 1),
                (SELECT res_id FROM ir_model_data WHERE module='account' AND name='group_account_manager'),
                (SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system')
         FROM res_company c ORDER BY c.id LIMIT 1",
        &[],
    ).await.unwrap();
    let company: i64 = data[0].get(0);
    let currency: i64 = data[0].get(1);
    let other_currency: i64 = data[0].get(2);
    let group: i64 = data[0].get(3);
    let administrator: i64 = data[0].get(4);
    let other_company = root
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut root,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Peer book company".into()))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let allowed = [company, other_company];
    let create_user = |login: &str, group: i64| {
        RawVals::new()
            .set_cell("name", Cell::Str(login.into()))
            .set_cell("login", Cell::Str(login.into()))
            .set_cell("company_id", Cell::Id(company))
            .set_commands(
                "company_ids",
                [Command::set(
                    allowed.map(|id| RecId::from_real(id).unwrap()),
                )],
            )
            .set_commands(
                "group_ids",
                [Command::set([RecId::from_real(group).unwrap()])],
            )
    };
    let users = root
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut root,
            vec![
                create_user("book_fixture_administrator", administrator),
                create_user("two_company_book_manager", group),
            ],
        )
        .await
        .unwrap();
    let user = users.ids()[1];
    root.commit().await.unwrap();
    drop(root);

    let policy = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let runtime = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let mut tx = runtime
        .begin_caller(UserId::new(user).unwrap())
        .await
        .unwrap();
    let scope = tx.env().with_context([(
        "allowed_company_ids",
        erp_sdk::Value::list(allowed.map(erp_sdk::Value::Int)),
    )]);
    let books = scope.records("stock.cost.book").unwrap();
    let book = books
        .create(
            &mut tx,
            vec![book_values(
                "Original book",
                company,
                currency,
                "management",
            )],
        )
        .await
        .unwrap();
    let peer = books
        .create(
            &mut tx,
            vec![book_values(
                "Peer book",
                other_company,
                currency,
                "management",
            )],
        )
        .await
        .unwrap();
    peer.check_access(&mut tx, Operation::Write).await.unwrap();
    book.check_access(&mut tx, Operation::Write).await.unwrap();

    for (field, changed) in [
        ("method", Cell::Str("fifo".into())),
        ("company_id", Cell::Id(other_company)),
        ("currency_id", Cell::Id(other_currency)),
        (
            "effective_from",
            Cell::Date(Date::from_ymd(2026, 1, 2).unwrap()),
        ),
    ] {
        let error = tx
            .savepoint(async |tx| {
                book.write_base(tx, RawVals::new().set_cell(field, changed))
                    .await?;
                tx.flush().await
            })
            .await
            .expect_err(field);
        assert_eq!(error.kind(), ErrorKind::Validation, "{field}: {error:?}");
        assert_eq!(
            error.constraint(),
            Some("stock_valuation_book_policy"),
            "{field}: {error:?}"
        );
        assert_eq!(error.sqlstate(), Some("ERP65"), "{field}: {error:?}");
        assert_eq!(
            error.message(),
            "Create a new cost book to change its company, purpose, method, currency or effective date."
        );
    }
    book.write_base(
        &mut tx,
        book_values("Renamed book", company, currency, "management")
            .set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    book.write(
        &mut tx,
        RawVals::new().set_cell("method", Cell::Str("lifo".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        book.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(company)
    );
    assert_eq!(
        book.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(currency)
    );
    assert_eq!(
        book.cell(&mut tx, "method").await.unwrap(),
        Cell::Str("lifo".into())
    );
    assert_eq!(
        book.cell(&mut tx, "name").await.unwrap(),
        Cell::Str("Renamed book".into())
    );
    assert_eq!(
        book.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(false)
    );
    tx.commit().await.unwrap();
    drop(tx);
    drop(runtime);
    drop(orm);
    drop(lifecycle);
    db.cleanup().await.unwrap();
}

fn book_values(name: &str, company: i64, currency: i64, purpose: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("company_id", Cell::Id(company))
        .set_cell("currency_id", Cell::Id(currency))
        .set_cell("purpose", Cell::Str(purpose.into()))
        .set_cell("method", Cell::Str("lifo".into()))
        .set_cell(
            "effective_from",
            Cell::Date(Date::from_ymd(2026, 1, 1).unwrap()),
        )
}

async fn verify_management_book(
    installed: &Orm,
    runtime: &Orm,
    company: i64,
    administrator: UserId,
    book_reader: UserId,
    stock_user: UserId,
) -> i64 {
    let mut root = installed.begin(SUPERUSER_ID).await.unwrap();
    let books = root.env().records("stock.cost.book").unwrap();
    assert!(
        books
            .search(&mut root, &erp_sdk::Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    let currency: i64 = root
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    root.rollback().await.unwrap();

    let mut caller = runtime.begin_caller(administrator).await.unwrap();
    let model = caller.env().records("stock.cost.book").unwrap();
    let book = model
        .create(
            &mut caller,
            vec![book_values(
                "Management LIFO",
                company,
                currency,
                "management",
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        book.cell(&mut caller, "currency_id").await.unwrap(),
        Cell::Id(currency)
    );
    let error = caller
        .savepoint(async |tx| {
            model
                .create(
                    tx,
                    vec![book_values(
                        "Statutory LIFO",
                        company,
                        currency,
                        "statutory",
                    )],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The database refused the change: Only management cost books can be configured until statutory posting is supported."
    );
    let error = caller
        .savepoint(async |tx| {
            book.write(
                tx,
                RawVals::new().set_cell("method", Cell::Str("fifo".into())),
            )
            .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Create a new cost book to change its company, purpose, method, currency or effective date."
    );
    book.write(
        &mut caller,
        RawVals::new().set_cell("name", Cell::Str("Management comparison".into())),
    )
    .await
    .unwrap();
    let id = book.ids()[0];
    caller.commit().await.unwrap();

    let mut reader = runtime.begin_caller(book_reader).await.unwrap();
    let readable = reader
        .env()
        .records("stock.cost.book")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        readable.cell(&mut reader, "name").await.unwrap(),
        Cell::Str("Management comparison".into())
    );
    let denied = readable
        .check_access(&mut reader, Operation::Write)
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    reader.rollback().await.unwrap();

    let mut clerk = runtime.begin_caller(stock_user).await.unwrap();
    let records = clerk.env().records("stock.cost.book").unwrap();
    for operation in [Operation::Read, Operation::Create] {
        let error = records
            .check_access(&mut clerk, operation)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
    }
    clerk.rollback().await.unwrap();
    id
}

// Called by the install/upgrade test after an unprivileged database policy has been built.
async fn verify_valuation_report(
    system: &Orm,
    runtime: &Orm,
    company: i64,
    product: i64,
    manager: UserId,
    clerk: UserId,
) {
    use erp_addon_reporting::runtime::{report_drill, report_lines, report_options};
    use serde_json::{Value as Json, json};
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let report = setup.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='report_stock_position'", &[]).await.unwrap()[0].get::<_,i64>(0);
    let place = setup.query("SELECT id FROM stock_location WHERE company_id=$1 AND usage='internal' ORDER BY id LIMIT 1", &[&company]).await.unwrap()[0].get::<_,i64>(0);
    let external = setup
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut setup,
            vec![RawVals::new().set_cell("name", Cell::Str("Consignment owner".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    let quants = setup.query("INSERT INTO stock_quant(company_id,product_id,location_id,owner_id,quantity,reserved_quantity,in_date) VALUES ($1,$2,$3,NULL,4,0,now()),($1,$2,$3,$4,19,0,now()) RETURNING id", &[&company,&product,&place,&external]).await.unwrap();
    let owned = quants[0].get::<_, i64>(0);
    let currency: i64 = setup
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    let foreign_company = setup
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell(
                        "name",
                        Cell::Str("Out of scope stock report company".into()),
                    )
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let foreign_location: i64 = setup
        .query(
            "INSERT INTO stock_location(name,usage,company_id)
         VALUES('Out of scope stock report location','internal',$1) RETURNING id",
            &[&foreign_company],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.execute(
        "INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date)
         VALUES ($1,$2,$3,100,0,now())",
        &[&foreign_company, &product, &foreign_location],
    ).await.unwrap();
    let quantity = stock_report_route(&mut setup).await;
    assert_ne!(quantity, report);
    let valuation_action = setup
        .query(
            "SELECT (a.params->>'report')::int8 FROM ir_act_client a
         JOIN ir_model_data x ON x.res_id=a.id AND x.model='ir.actions.client'
         WHERE x.module='stock_account' AND x.name='action_report_stock_valuation'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(valuation_action[0].get::<_, i64>(0), report);
    let menus = setup
        .query(
            "SELECT name, res_id FROM ir_model_data
         WHERE (module, name) IN (('stock', 'menu_report_stock_position'),
                                  ('stock', 'menu_report_moves_analysis'),
                                  ('stock', 'menu_stock_quant'),
                                  ('stock_account', 'menu_report_stock_valuation'))
         ORDER BY name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(menus.len(), 4);
    setup.commit().await.unwrap();
    drop(setup);
    for (user, expected) in [(manager, true), (clerk, false)] {
        let mut tx = runtime.begin_caller(user).await.unwrap();
        let env = tx.env();
        let form = erp_addon_base::ir_ui_view::select::ViewType::new(&tx, "form").unwrap();
        let request = erp_addon_base::ir_ui_view::select::ViewRequest::new("stock.quant", &form);
        let view = erp_addon_base::ir_ui_view::get_view(&mut tx, &env, &request)
            .await
            .unwrap();
        assert_eq!(
            view.arch().contains("stock_account.issue_cost_preview"),
            expected
        );
        let visible = erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&env, &mut tx, false)
            .await
            .unwrap();
        for menu in &menus {
            let name: &str = menu.get(0);
            let id: i64 = menu.get(1);
            assert_eq!(
                visible.contains(&id),
                name == "menu_report_stock_position" || expected,
                "visibility of {name}"
            );
        }

        let mut options = report_options(&env, &mut tx, quantity, &Json::Null)
            .await
            .unwrap()["options"]
            .clone();
        let labels: Vec<_> = options["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|column| column["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["on_hand", "reserved", "available"]);
        options["unfold_all"] = json!(true);
        let rows = report_lines(&env, &mut tx, &options).await.unwrap();
        assert!(
            rows["lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["name"] != "Out of scope stock report location")
        );
        let amounts = rows["lines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "Valuation fixture")
            .unwrap()["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cell| {
                cell["value"]
                    .as_str()
                    .unwrap()
                    .parse::<erp_sdk::Decimal>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(amounts, [23, 0, 23].map(erp_sdk::Decimal::from));
        for format in ["pdf", "xlsx"] {
            let body = json!({"report": quantity, "options": options, "format": format});
            let file = erp_addon_reporting::export::prepare(&mut tx, body.to_string().as_bytes())
                .await
                .unwrap();
            assert_eq!(file.file_name(), format!("Stock Report.{format}"));
            if format == "pdf" {
                use erp_report::LayoutEngine as _;
                let document = report_frame::through_frame(&file.document().unwrap());
                let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/report_fonts");
                let engine =
                    erp_report_typst::TypstEngine::new(&fonts, erp_report::Limits::default())
                        .unwrap();
                let rendered = engine.render(&document).unwrap();
                let pdf = lopdf::Document::load_mem(rendered.bytes()).unwrap();
                let text = pdf.extract_text(&[1]).unwrap();
                assert!(
                    text.split_whitespace()
                        .collect::<String>()
                        .contains("Valuationfixture")
                );
            } else {
                let workbook = file.xlsx().unwrap();
                assert!(workbook.bytes.starts_with(b"PK\x03\x04"));
            }
        }
        tx.rollback().await.unwrap();
    }
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let env = tx.env();
    let options = report_options(&env, &mut tx, report, &Json::Null)
        .await
        .unwrap()["options"]
        .clone();
    let col = options["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["label"] == "value")
        .unwrap();
    let missing = report_lines(&env, &mut tx, &options).await.unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::Validation);
    assert!(
        missing.message().contains("no published valuation"),
        "{}",
        missing.message()
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'average','cost_scale_v1',30,10,0,currency_id,0.01,1 FROM res_company WHERE id=$1", &[&company,&product]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let env = tx.env();
    let result = report_lines(&env, &mut tx, &options).await.unwrap();
    let root = &result["lines"][0];
    assert_eq!(
        root["cells"][col]["value"]
            .as_str()
            .unwrap()
            .parse::<erp_sdk::Decimal>()
            .unwrap(),
        erp_sdk::Decimal::from(12)
    );
    let action = report_drill(
        &env,
        &mut tx,
        &options,
        root["id"].as_str().unwrap(),
        &json!({"cell":col}),
    )
    .await
    .unwrap();
    let rows = env
        .records("stock.quant")
        .unwrap()
        .search(
            &mut tx,
            &erp_sdk::Domain::from_json(&action["domain"]).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rows.ids(), vec![owned]);
    tx.rollback().await.unwrap();
    drop(tx);

    // The allocation fits in the report although multiplying units and value first does not.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stock_quant SET quantity=1000000000000000 WHERE id=$1",
            &[&owned],
        )
        .await
        .unwrap();
    setup.execute("UPDATE stk_cost_state SET grid_value=800000000000000.60,quantity=2000000000000000 WHERE company_id=$1 AND product_id=$2",
        &[&company, &product]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let result = report_lines(&tx.env(), &mut tx, &options).await.unwrap();
    let amount: erp_sdk::Decimal = result["lines"][0]["cells"][col]["value"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(amount, "400000000000000.30".parse().unwrap());
    tx.rollback().await.unwrap();
    drop(tx);

    // G256 state exceeds Decimal, but its tiny exact quant share has a cash result.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stock_quant SET quantity=0.000000000000000001 WHERE id=$1",
            &[&owned],
        )
        .await
        .unwrap();
    setup.execute("UPDATE stk_cost_state SET grid_value=10000000000000000000000000000000000000000,quantity=1000000000000000000 WHERE company_id=$1 AND product_id=$2", &[&company,&product]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let result = report_lines(&tx.env(), &mut tx, &options).await.unwrap();
    assert_eq!(
        result["lines"][0]["cells"][col]["value"]
            .as_str()
            .unwrap()
            .parse::<erp_sdk::Decimal>()
            .unwrap(),
        erp_sdk::Decimal::from(10000)
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // A truly oversized displayed amount still fails at the report cash boundary.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stock_quant SET quantity=1000000000000000000 WHERE id=$1",
            &[&owned],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let error = report_lines(&tx.env(), &mut tx, &options)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The monetary value is out of the supported range (quantize)."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Two eligible products each contribute 0.02. With a 0.05 increment the total is 0.05.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup.execute("UPDATE res_currency SET rounding=0.05 WHERE id=(SELECT currency_id FROM res_company WHERE id=$1)", &[&company]).await.unwrap();
    setup
        .execute("UPDATE stock_quant SET quantity=1 WHERE id=$1", &[&owned])
        .await
        .unwrap();
    setup.execute("UPDATE stk_cost_state SET grid_value=0.06,quantity=3 WHERE company_id=$1 AND product_id=$2", &[&company,&product]).await.unwrap();
    let second_template: i64 = setup.query("INSERT INTO product_template(name,uom_id,type,tracking) SELECT '{\"en_US\":\"Another valued product\"}',id,'consu','none' FROM uom_uom ORDER BY id LIMIT 1 RETURNING id", &[]).await.unwrap()[0].get(0);
    let second: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&second_template],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.execute("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,1,0,now())", &[&company,&second,&place]).await.unwrap();
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'fifo','cost_scale_v1',0.06,3,0,currency_id,0.05,1 FROM res_company WHERE id=$1", &[&company,&second]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let result = report_lines(&tx.env(), &mut tx, &options).await.unwrap();
    assert_eq!(
        result["lines"][0]["cells"][col]["value"]
            .as_str()
            .unwrap()
            .parse::<erp_sdk::Decimal>()
            .unwrap(),
        "0.05".parse().unwrap()
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut tx = runtime.begin_caller(clerk).await.unwrap();
    let env = tx.env();
    let options = report_options(&env, &mut tx, report, &Json::Null)
        .await
        .unwrap()["options"]
        .clone();
    assert!(
        options["columns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["label"] != "value")
    );
    assert_eq!(
        report_lines(&env, &mut tx, &options).await.unwrap()["lines"][0]["cells"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    tx.rollback().await.unwrap();
    drop(tx);
    verify_more_rounding(system, runtime, company, manager, clerk, product).await;
}

async fn verify_more_rounding(
    system: &Orm,
    runtime: &Orm,
    company: i64,
    manager: UserId,
    clerk: UserId,
    absent_product: i64,
) {
    use erp_addon_reporting::runtime::{report_expand, report_lines, report_options};
    use serde_json::{Value as Json, json};
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let report:i64=setup.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='report_stock_position'",&[]).await.unwrap()[0].get(0);
    let definition:i64=setup.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='report_stock_position_line'",&[]).await.unwrap()[0].get(0);
    let place:i64=setup.query("SELECT id FROM stock_location WHERE company_id=$1 AND usage='internal' ORDER BY id LIMIT 1",&[&company]).await.unwrap()[0].get(0);
    let template:i64=setup.query("INSERT INTO product_template(name,uom_id,type,tracking) SELECT '{\"en_US\":\"Rounding tail product\"}',id,'consu','none' FROM uom_uom ORDER BY id LIMIT 1 RETURNING id",&[]).await.unwrap()[0].get(0);
    let product: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let currency: i64 = setup
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    let owner = setup
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut setup,
            vec![RawVals::new().set_cell("name", Cell::Str("Tail consignment".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    // Eleven named keys plus the NULL key. Sorting by ordinary record count puts NULL
    // last (two records), independently of the SQL backend's NULL ordering.
    let mut lots = Vec::new();
    for index in 0..11 {
        let name = format!("Rounding lot {index:02}");
        let lot: i64 = setup
            .query(
                "INSERT INTO stock_lot(name,product_id,company_id) VALUES($1,$2,$3) RETURNING id",
                &[&name, &product, &company],
            )
            .await
            .unwrap()[0]
            .get(0);
        let quantity = if index == 0 { 1i64 } else { 0 };
        setup.execute("INSERT INTO stock_quant(company_id,product_id,location_id,lot_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,$4,$5::int8::numeric,0,now())",&[&company,&product,&place,&lot,&quantity]).await.unwrap();
        lots.push(lot);
    }
    setup.execute("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,1,0,now())",&[&company,&product,&place]).await.unwrap();
    setup.execute("INSERT INTO stock_quant(company_id,product_id,location_id,owner_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,$4,999,0,now())",&[&company,&product,&place,&owner]).await.unwrap();
    setup.execute("UPDATE res_currency SET rounding=0.01 WHERE id=(SELECT currency_id FROM res_company WHERE id=$1)",&[&company]).await.unwrap();
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'average','cost_scale_v1',0.008,2,0,currency_id,0.01,1 FROM res_company WHERE id=$1",&[&company,&product]).await.unwrap();
    let domain = json!([
        ["product_id", "=", product],
        ["location_id.usage", "=", "internal"]
    ])
    .to_string();
    setup.execute("UPDATE reporting_report_expression SET domain=$2 WHERE line_id=$1 AND engine IN ('aggregate','native')",&[&definition,&domain]).await.unwrap();
    setup
        .env()
        .records("reporting.report")
        .unwrap()
        .browse([report])
        .unwrap()
        .write(
            &mut setup,
            RawVals::new().set_cell("load_more_limit", Cell::Int(10)),
        )
        .await
        .unwrap();
    setup
        .env()
        .records("reporting.report.column")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("report_id", Cell::Id(report))
                    .set_cell("name", Cell::Str("Positions".into()))
                    .set_cell("label", Cell::Str("positions".into()))
                    .set_cell("sequence", Cell::Int(50))
                    .set_cell("kind", Cell::Str("integer".into()))
                    .set_cell("sortable", Cell::Bool(true)),
                RawVals::new()
                    .set_cell("report_id", Cell::Id(report))
                    .set_cell("name", Cell::Str("Null-lot value".into()))
                    .set_cell("label", Cell::Str("null_value".into()))
                    .set_cell("sequence", Cell::Int(60))
                    .set_cell("kind", Cell::Str("monetary".into()))
                    .set_cell("groups", Cell::Str("stock.group_stock_manager".into())),
            ],
        )
        .await
        .unwrap();
    setup
        .env()
        .records("reporting.report.expression")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("line_id", Cell::Id(definition))
                    .set_cell("label", Cell::Str("positions".into()))
                    .set_cell("model", Cell::Str("stock.quant".into()))
                    .set_cell("domain", Cell::Str(domain.into())),
                RawVals::new()
                    .set_cell("line_id", Cell::Id(definition))
                    .set_cell("label", Cell::Str("null_value".into()))
                    .set_cell("engine", Cell::Str("native".into()))
                    .set_cell("function", Cell::Str("stock_account.quant_value".into()))
                    .set_cell("model", Cell::Str("stock.quant".into()))
                    .set_cell(
                        "domain",
                        Cell::Str(
                            json!([["product_id", "=", product], ["lot_id", "=", false]])
                                .to_string()
                                .into(),
                        ),
                    ),
            ],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let env = tx.env();
    let mut options = report_options(&env, &mut tx, report, &Json::Null)
        .await
        .unwrap()["options"]
        .clone();
    options["order"] = json!({"column":"positions","direction":"asc"});
    let index = |label: &str| {
        options["columns"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["label"] == label)
            .unwrap()
    };
    let value = index("value");
    let null_value = index("null_value");
    let positions = index("positions");
    let quantity = index("on_hand");
    let available = index("available");
    let amount = |row: &Json, column: usize| {
        row["cells"][column]["value"]
            .as_str()
            .unwrap()
            .parse::<erp_sdk::Decimal>()
            .unwrap()
    };
    let top = report_lines(&env, &mut tx, &options).await.unwrap();
    assert_eq!(amount(&top["lines"][0], value), "0.01".parse().unwrap());
    let root = top["lines"][0]["id"].as_str().unwrap().to_owned();
    let location = report_expand(&env, &mut tx, &options, &root, 0)
        .await
        .unwrap()["lines"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let parent = report_expand(&env, &mut tx, &options, &location, 0)
        .await
        .unwrap()["lines"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let page = report_expand(&env, &mut tx, &options, &parent, 0)
        .await
        .unwrap();
    let rows = page["lines"].as_array().unwrap();
    assert_eq!(rows.len(), 11);
    assert!(
        rows[0]["id"]
            .as_str()
            .unwrap()
            .ends_with(&format!("~{}", lots[0]))
    );
    let more = &rows[10];
    assert_eq!(more["kind"], "more");
    assert_eq!(more["remaining"], 2);
    assert_eq!(more["next_offset"], 10);
    assert_eq!(more["cells"][value]["currency"], json!(currency));
    assert_eq!(more["cells"][value]["scale"], 2);
    assert_eq!(
        amount(more, value),
        erp_sdk::Decimal::ZERO,
        "Only the remaining owned 0.004 share is rounded, once, to 0.00."
    );
    assert_eq!(amount(more, null_value), erp_sdk::Decimal::ZERO);
    assert_eq!(amount(more, quantity), erp_sdk::Decimal::from(1000));
    assert_eq!(amount(more, available), erp_sdk::Decimal::from(1000));
    assert_eq!(amount(more, positions), erp_sdk::Decimal::from(3));
    // Offset1 loads eleven cumulative named keys; its complement is the NULL key only.
    let overlap = report_expand(&env, &mut tx, &options, &parent, 1)
        .await
        .unwrap();
    let more = overlap["lines"].as_array().unwrap().last().unwrap();
    assert_eq!(more["kind"], "more");
    assert_eq!(more["remaining"], 1);
    assert_eq!(more["next_offset"], 11);
    assert_eq!(amount(more, value), erp_sdk::Decimal::ZERO);
    assert_eq!(amount(more, quantity), erp_sdk::Decimal::from(1000));
    assert_eq!(amount(more, positions), erp_sdk::Decimal::from(2));
    let final_page = report_expand(&env, &mut tx, &options, &parent, 10)
        .await
        .unwrap();
    let final_rows = final_page["lines"].as_array().unwrap();
    assert_eq!(final_rows.len(), 2);
    assert!(final_rows.iter().all(|row| row["kind"] != "more"));
    assert_eq!(
        final_rows[1]["id"],
        format!("{parent}|group:lot_id~stock.lot~")
    );
    assert_eq!(amount(&final_rows[1], value), erp_sdk::Decimal::ZERO);
    assert!(
        report_expand(&env, &mut tx, &options, &parent, 12)
            .await
            .unwrap()["lines"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let empty_parent = format!("{location}|group:product_id~product.product~{absent_product}");
    assert!(
        report_expand(&env, &mut tx, &options, &empty_parent, 0)
            .await
            .unwrap()["lines"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut restored = options.clone();
    restored["unfolded"] = json!([root, location, parent]);
    restored["loaded_pages"] = json!({(&parent):1});
    let restored = report_lines(&env, &mut tx, &restored).await.unwrap();
    assert!(
        restored["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] != "more")
    );
    assert_eq!(
        amount(&restored["lines"][0], value),
        "0.01".parse().unwrap()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // NULL is now in the loaded prefix. Larger per-share values expose any accidental
    // re-inclusion of this loaded NULL key in the native remaining-domain calculation.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_state SET grid_value=0.03 WHERE company_id=$1 AND product_id=$2",
            &[&company, &product],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let env = tx.env();
    let mut descending = options.clone();
    descending["order"] = json!({"column":"positions","direction":"desc"});
    let page = report_expand(&env, &mut tx, &descending, &parent, 0)
        .await
        .unwrap();
    let rows = page["lines"].as_array().unwrap();
    assert_eq!(rows[0]["id"], format!("{parent}|group:lot_id~stock.lot~"));
    let more = rows.last().unwrap();
    assert_eq!(more["remaining"], 2);
    assert_eq!(amount(more, value), erp_sdk::Decimal::ZERO);
    assert_eq!(amount(more, null_value), erp_sdk::Decimal::ZERO);
    assert_eq!(amount(more, quantity), erp_sdk::Decimal::ZERO);
    assert_eq!(amount(more, positions), erp_sdk::Decimal::from(2));
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = runtime.begin_caller(clerk).await.unwrap();
    let env = tx.env();
    let options = report_options(&env, &mut tx, report, &Json::Null)
        .await
        .unwrap()["options"]
        .clone();
    assert!(
        options["columns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["label"] != "value" && c["label"] != "null_value")
    );
    tx.rollback().await.unwrap();
}

// Partial preview acceptance only: no receipt writer or accounting posting is exercised.
async fn verify_receipt_preview(
    system: &Orm,
    db: &TestDb,
    company: i64,
    manager: UserId,
    clerk: UserId,
) {
    use serde_json::{Value as Json, json};
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let template: i64 = setup.query("INSERT INTO product_template(name,uom_id,type,tracking) SELECT '{\"en_US\":\"Receipt preview\"}',id,'consu','none' FROM uom_uom ORDER BY id LIMIT 1 RETURNING id", &[]).await.unwrap()[0].get(0);
    let product: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let place: i64 = setup.query("SELECT id FROM stock_location WHERE company_id=$1 AND usage='internal' ORDER BY id LIMIT 1", &[&company]).await.unwrap()[0].get(0);
    let quant: i64 = setup.query("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,12,0,now()) RETURNING id", &[&company,&product,&place]).await.unwrap()[0].get(0);
    let mut sources = Vec::new();
    for component in ["earlier", "later"] {
        let source: i64 = setup.query("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) VALUES($1,$2,'product.product',$2,$3,0,'Movement','','2026-01-01','2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-fixture') RETURNING id", &[&company,&product,&component]).await.unwrap()[0].get(0);
        sources.push(source);
    }
    // Deliberately reverse layer ID order at tied timestamps. Source sequence must decide.
    let later: i64 = setup.query("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES($1,$2,$3,'receipt','2026-01-01',5,85,0,0,0,1,1) RETURNING id", &[&company,&product,&sources[1]]).await.unwrap()[0].get(0);
    let earlier: i64 = setup.query("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES($1,$2,$3,'receipt','2026-01-01',7,77,0,0,0,1,1) RETURNING id", &[&company,&product,&sources[0]]).await.unwrap()[0].get(0);
    assert!(earlier > later && sources[0] < sources[1]);
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'fifo','cost_scale_v1',162,12,0,currency_id,0.01,1 FROM res_company WHERE id=$1", &[&company,&product]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    let currency = setup
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let books = setup.env().records("stock.cost.book").unwrap();
    let foreign_company = setup
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Preview foreign company".into()))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let other_currency: i64 = setup
        .query(
            "SELECT id FROM res_currency WHERE id <> $1 ORDER BY id LIMIT 1",
            &[&currency],
        )
        .await
        .unwrap()[0]
        .get(0);
    let selected = books
        .create(
            &mut setup,
            vec![
                book_values("Receipt LIFO comparison", company, currency, "management"),
                book_values("Receipt FIFO comparison", company, currency, "management")
                    .set_cell("method", Cell::Str("fifo".into())),
                book_values("Future comparison", company, currency, "management").set_cell(
                    "effective_from",
                    Cell::Date(Date::from_ymd(2199, 1, 1).unwrap()),
                ),
                book_values("Archived comparison", company, currency, "management")
                    .set_cell("active", Cell::Bool(false)),
                book_values(
                    "Foreign-company comparison",
                    foreign_company,
                    currency,
                    "management",
                ),
                book_values(
                    "Foreign-currency comparison",
                    company,
                    other_currency,
                    "management",
                ),
            ],
        )
        .await
        .unwrap();
    let book_ids = selected.ids();
    setup.commit().await.unwrap();
    drop(setup);
    let policy = PolicySource::build(Arc::clone(system.registry()), db.pool().clone())
        .await
        .unwrap();
    let runtime = Orm::builder(Arc::clone(system.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(system.registry()).unwrap())
        .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let request = |method: &str, quantity: &str| json!({"scope":"primary_unbooked_receipt_projection", "method":method,"quantity":quantity});
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    for (method, amount, first, source) in [
        ("fifo", "66", earlier, sources[0]),
        ("lifo", "96", later, sources[1]),
    ] {
        let result: Json = preview_issue(&record, &mut tx, request(method, "6"))
            .await
            .unwrap();
        assert_eq!(result["scope"], "primary_unbooked_receipt_projection");
        assert_eq!(result["comparison_only"], true);
        assert_eq!(result["quantity_scope"], "company_product_lot_cost_pool");
        assert_eq!(result["location_availability_checked"], false);
        assert_eq!(result["cost_book"], Json::Null);
        assert_eq!(
            result["cost_key"],
            json!({"company_id":company,"product_id":product,"lot_id":null})
        );
        assert_eq!(
            result["total"],
            json!({"numerator":amount,"denominator":"1"})
        );
        assert_eq!(
            result["pool_remaining_value"],
            json!({"numerator":if method=="fifo" {"96"} else {"66"},"denominator":"1"})
        );
        assert_eq!(
            result["pool_remaining_quantity"],
            json!({"numerator":"6","denominator":"1"})
        );
        assert_eq!(result["allocations"][0]["layer_id"], first);
        assert_eq!(result["allocations"][0]["source_sequence"], source);
        assert_eq!(result["allocations"][0]["source_id"], source);
        assert_eq!(
            result["allocations"][0]["origin"],
            json!({"model":"product.product","id":product,"component": if method=="fifo" {"earlier"} else {"later"},"revision":0})
        );
    }
    let full: Json = preview_issue(&record, &mut tx, request("fifo", "12"))
        .await
        .unwrap();
    assert_eq!(full["total"], json!({"numerator":"162","denominator":"1"}));
    let fingerprint = "SELECT jsonb_build_object(
        'layers',(SELECT jsonb_agg(to_jsonb(l) ORDER BY id) FROM stk_cost_layer l WHERE product_id=$1),
        'state',(SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM stk_cost_state s WHERE product_id=$1),
        'sources',(SELECT jsonb_agg(to_jsonb(v) ORDER BY id) FROM stk_value_source v WHERE product_id=$1),
        'receipts',(SELECT jsonb_agg(to_jsonb(r) ORDER BY id) FROM stk_value_receipt r WHERE product_id=$1),
        'consumptions',(SELECT jsonb_agg(to_jsonb(c) ORDER BY id) FROM stk_layer_consumption c WHERE product_id=$1),
        'journal',(SELECT jsonb_agg(to_jsonb(j) ORDER BY id) FROM stk_value_journal j WHERE product_id=$1),
        'quants',(SELECT jsonb_agg(to_jsonb(q) ORDER BY id) FROM stock_quant q WHERE product_id=$1),
        'account_moves',(SELECT count(*) FROM account_move))";
    let snapshot: Json = tx.query(fingerprint, &[&product]).await.unwrap()[0].get(0);
    let book_request = |id: i64, quantity: &str| json!({"scope":"management_cost_book","cost_book_id":id,"quantity":quantity});
    let fraction = |numerator: i64, denominator: i64| json!({"numerator":numerator.to_string(),"denominator":denominator.to_string()});
    // Deliberately compare every component against the authored expectation. These
    // are reusable expectations for future durable issue tests, not issued rows.
    let lifo_lines = [
        (later, sources[1], "later", 5, 85, 0, 0),
        (earlier, sources[0], "earlier", 1, 11, 6, 66),
    ];
    let fifo_lines = [(earlier, sources[0], "earlier", 6, 66, 1, 11)];
    for (index, method, issued, remaining, expected) in [
        (0, "lifo", 96, 66, lifo_lines.as_slice()),
        (1, "fifo", 66, 96, fifo_lines.as_slice()),
    ] {
        let book = book_ids[index];
        let result = preview_issue(&record, &mut tx, book_request(book, "6"))
            .await
            .unwrap();
        assert_eq!(result["scope"], "management_cost_book");
        assert_eq!(
            result["receipt_scope"],
            "primary_unbooked_receipt_projection"
        );
        assert_eq!(
            result["cost_book"],
            json!({"id":book,"purpose":"management"})
        );
        assert_eq!(result["comparison_only"], true);
        assert_eq!(result["method"], method);
        assert_eq!(result["total"], fraction(issued, 1));
        assert_eq!(result["pool_remaining_quantity"], fraction(6, 1));
        assert_eq!(result["pool_remaining_value"], fraction(remaining, 1));
        let allocations = result["allocations"].as_array().unwrap();
        assert_eq!(allocations.len(), expected.len());
        for (allocation, &(layer, source, component, quantity, value, left, left_value)) in
            allocations.iter().zip(expected)
        {
            assert_eq!(allocation["layer_id"], layer);
            assert_eq!(allocation["source_id"], source);
            assert_eq!(allocation["source_sequence"], source);
            assert_eq!(
                allocation["origin"],
                json!({"model":"product.product","id":product,"component":component,"revision":0})
            );
            assert_eq!(allocation["quantity"], fraction(quantity, 1));
            assert_eq!(allocation["value"], fraction(value, 1));
            assert_eq!(allocation["remaining_quantity"], fraction(left, 1));
            assert_eq!(allocation["remaining_value"], fraction(left_value, 1));
        }
    }
    let inactive = "Choose an active management cost book effective on the company business date.";
    let inaccessible = "Issue preview requires a readable cost book for the stock line's company.";
    for (id, kind, message) in [
        (book_ids[2], ErrorKind::Validation, inactive),
        (book_ids[3], ErrorKind::Validation, inactive),
        (book_ids[4], ErrorKind::Access, inaccessible),
        (
            book_ids[5],
            ErrorKind::Validation,
            "Management issue preview requires a cost book in the company currency.",
        ),
        (i64::MAX, ErrorKind::Access, inaccessible),
    ] {
        let error = preview_issue(&record, &mut tx, book_request(id, "6"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), message);
    }
    for (input, message) in [
        (
            request("fifo", "13"),
            "The receipt cost pool does not cover the requested issue quantity.",
        ),
        (
            book_request(book_ids[0], "13"),
            "The receipt cost pool does not cover the requested issue quantity.",
        ),
        (
            request("fifo", "0"),
            "Issue preview quantity must be greater than zero.",
        ),
        (
            request("fifo", "-1"),
            "Issue preview quantity must be greater than zero.",
        ),
        (
            request("average", "1"),
            "Issue preview method must be fifo or lifo.",
        ),
        (
            json!({"scope":"book","method":"fifo","quantity":"1"}),
            "Issue preview scope must be primary_unbooked_receipt_projection or management_cost_book.",
        ),
    ] {
        let error = preview_issue(&record, &mut tx, input).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
    }
    let unchanged: Json = tx.query(fingerprint, &[&product]).await.unwrap()[0].get(0);
    assert_eq!(snapshot, unchanged);
    tx.rollback().await.unwrap();
    drop(tx);
    verify_pending_preview_origins(system, &runtime, manager, quant, company, product).await;

    // Quantity alone does not establish the published value: refuse unexplained adjustments.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_state SET grid_value=180 WHERE company_id=$1 AND product_id=$2",
            &[&company, &product],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "6"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview receipt values do not reconcile to the published cost state; valuation-only adjustments are not supported."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_state SET grid_value=162 WHERE company_id=$1 AND product_id=$2",
            &[&company, &product],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(clerk).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Only stock managers may preview issue costs."
    );
    let storage = tx.env().records("stock.valuation.layer").unwrap();
    assert_eq!(
        storage
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Access
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Existing partial consumption changes the financial pool, not the selected location quant.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_layer SET consumed_quantity=2,consumed_value=22 WHERE id=$1",
            &[&earlier],
        )
        .await
        .unwrap();
    setup.execute("UPDATE stk_cost_state SET quantity=10,grid_value=140 WHERE company_id=$1 AND product_id=$2", &[&company,&product]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let partial: Json = preview_issue(&record, &mut tx, request("fifo", "6"))
        .await
        .unwrap();
    assert_eq!(
        partial["total"],
        json!({"numerator":"72","denominator":"1"})
    );
    assert_eq!(
        partial["allocations"][0]["quantity"],
        json!({"numerator":"5","denominator":"1"})
    );
    assert_eq!(
        partial["pool_remaining_quantity"],
        json!({"numerator":"4","denominator":"1"})
    );
    assert_eq!(
        partial["pool_remaining_value"],
        json!({"numerator":"68","denominator":"1"})
    );
    let named_partial = preview_issue(&record, &mut tx, book_request(book_ids[1], "6"))
        .await
        .unwrap();
    assert_eq!(named_partial["allocations"], partial["allocations"]);
    assert_eq!(named_partial["total"], fraction(72, 1));
    tx.rollback().await.unwrap();
    drop(tx);
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_layer SET consumed_quantity=7,consumed_value=77 WHERE id=$1",
            &[&earlier],
        )
        .await
        .unwrap();
    setup.execute("UPDATE stk_cost_state SET quantity=5,grid_value=85 WHERE company_id=$1 AND product_id=$2", &[&company,&product]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let exhausted: Json = preview_issue(&record, &mut tx, request("fifo", "5"))
        .await
        .unwrap();
    assert_eq!(exhausted["allocations"].as_array().unwrap().len(), 1);
    assert_eq!(exhausted["allocations"][0]["layer_id"], later);
    assert_eq!(
        exhausted["total"],
        json!({"numerator":"85","denominator":"1"})
    );
    let named_exhausted = preview_issue(&record, &mut tx, book_request(book_ids[1], "5"))
        .await
        .unwrap();
    assert_eq!(named_exhausted["allocations"], exhausted["allocations"]);
    assert_eq!(named_exhausted["pool_remaining_value"], fraction(0, 1));
    tx.rollback().await.unwrap();
    drop(tx);

    // Corrupt publication generation must refuse rather than assembling a mixed pool.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_state SET generation=2 WHERE company_id=$1 AND product_id=$2",
            &[&company, &product],
        )
        .await
        .unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview cost state and receipt layers have different generations."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // A canonical per-layer residue survives preview and exact final clearance.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let fractional: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let fractional_quant: i64 = setup.query("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,2,0,now()) RETURNING id", &[&company,&fractional,&place]).await.unwrap()[0].get(0);
    let source: i64 = setup.query("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) VALUES($1,$2,'product.product',$2,'fractional',0,'Movement','','2026-01-01','2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-fraction') RETURNING id", &[&company,&fractional]).await.unwrap()[0].get(0);
    setup.execute("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES($1,$2,$3,'receipt','2026-01-01',3,1,1,0.333333333333333333,1,3000000000000000000,1)", &[&company,&fractional,&source]).await.unwrap();
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'fifo','cost_scale_v1',0.666666666666666667,2,0,currency_id,0.01,1 FROM res_company WHERE id=$1", &[&company,&fractional]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([fractional_quant])
        .unwrap();
    for (quantity, numerator) in [("1", "1"), ("2", "2")] {
        let result: Json = preview_issue(&record, &mut tx, request("fifo", quantity))
            .await
            .unwrap();
        assert_eq!(
            result["total"],
            json!({"numerator":numerator,"denominator":"3"})
        );
        assert_eq!(
            result["allocations"][0]["prior_consumed_residual"],
            json!({"numerator":"1","denominator":"3000000000000000000"})
        );
        let named = preview_issue(&record, &mut tx, book_request(book_ids[0], quantity))
            .await
            .unwrap();
        assert_eq!(named["total"], result["total"]);
        assert_eq!(named["allocations"], result["allocations"]);
        let remaining = if quantity == "1" {
            fraction(1, 3)
        } else {
            fraction(0, 1)
        };
        assert_eq!(named["pool_remaining_value"], remaining);
    }
    tx.rollback().await.unwrap();
    drop(tx);

    // Even an UNALLOCATED inaccessible origin must prevent aggregate remaining-pool leakage.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let private_source: i64 = setup.query("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) VALUES($1,$2,'stock.valuation.source',$3,'private-origin',0,'Movement','','2026-01-01','2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-private') RETURNING id", &[&company,&fractional,&source]).await.unwrap()[0].get(0);
    setup.execute("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES($1,$2,$3,'receipt','2026-01-01',1,2,0,0,0,1,1)", &[&company,&fractional,&private_source]).await.unwrap();
    setup.execute("UPDATE stk_cost_state SET quantity=3,grid_value=2.666666666666666667 WHERE company_id=$1 AND product_id=$2", &[&company,&fractional]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([fractional_quant])
        .unwrap();
    let denied = tx
        .env()
        .records("stock.valuation.source")
        .unwrap()
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    for quantity in ["1", "40"] {
        let error = preview_issue(&record, &mut tx, request("fifo", quantity))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Issue preview requires readable original cost source records."
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);

    // A different cost partition cannot be silently pooled into the admitted primary scope.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "UPDATE stk_cost_state SET generation=1 WHERE company_id=$1 AND product_id=$2",
            &[&company, &product],
        )
        .await
        .unwrap();
    let separate: i64 = setup.query("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) VALUES($1,$2,'product.product',$2,'partition',0,'Movement','management','2026-01-01','2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-partition') RETURNING id", &[&company,&product]).await.unwrap()[0].get(0);
    setup.execute("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES($1,$2,$3,'receipt','2026-01-01',1,1,0,0,0,1,1)", &[&company,&product,&separate]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview supports only primary unbooked FIFO receipt sources with matching policy and effective time."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // A consigned anchor fails before cost projection reads, even for a manager.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let owner = setup
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut setup,
            vec![RawVals::new().set_cell("name", Cell::Str("Preview third party".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    setup
        .execute(
            "UPDATE stock_quant SET owner_id=$2 WHERE id=$1",
            &[&quant, &owner],
        )
        .await
        .unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT s.id,s.company_id,s.product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source s WHERE s.company_id=$1 AND NOT EXISTS(SELECT 1 FROM stk_value_receipt r WHERE r.source_id=s.id)", &[&company]).await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview requires company-owned stock in an internal or transit location."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // The same caller's ordinary read refusal is preserved for another company's anchor.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let foreign_location: i64 = setup.query("INSERT INTO stock_location(name,usage,company_id) VALUES('Preview other company','internal',$1) RETURNING id", &[&foreign_company]).await.unwrap()[0].get(0);
    let foreign_quant: i64 = setup.query("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,1,0,now()) RETURNING id", &[&foreign_company,&product,&foreign_location]).await.unwrap()[0].get(0);
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([foreign_quant])
        .unwrap();
    let ordinary = record
        .read_with(&mut tx, &["company_id"], erp_sdk::Load::Raw)
        .await
        .unwrap_err();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), ordinary.message());
    tx.rollback().await.unwrap();
    drop(tx);

    // An administrator's real source-row bound refuses before any truncated comparison.
    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let bounded: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let bounded_quant: i64 = setup.query("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,101,0,now()) RETURNING id", &[&company,&bounded,&place]).await.unwrap()[0].get(0);
    let applied_origin: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.execute("WITH sources AS(INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) SELECT $1,$2,'product.product',CASE WHEN i=1 THEN $3::bigint ELSE $2::bigint END,i::text,0,'Movement','','2026-01-01','2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-bound' FROM generate_series(1,101) i RETURNING id) INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) SELECT $1,$2,id,'receipt','2026-01-01',1,1,0,0,0,1,1 FROM sources", &[&company,&bounded,&applied_origin]).await.unwrap();
    setup.execute("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,applied_generation,applied_at,recorded_at) SELECT id,company_id,product_id,'applied','inline',1,'2026-01-01','2026-01-01' FROM stk_value_source WHERE product_id=$1", &[&bounded]).await.unwrap();
    setup.execute("INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT id,$2,'fifo','cost_scale_v1',101,101,0,currency_id,0.01,1 FROM res_company WHERE id=$1", &[&company,&bounded]).await.unwrap();
    let env = setup.env();
    let key = "reporting.export_max_rows".to_owned();
    let scope = erp_sdk::views::wire::SettingsScope::Global {};
    let revision = serde_json::to_value(
        erp_addon_base::ir_config_setting::read_values(
            &env,
            &mut setup,
            scope.clone(),
            vec![key.clone()],
        )
        .await
        .unwrap(),
    )
    .unwrap();
    erp_addon_base::ir_config_setting::write_values(
        &env,
        &mut setup,
        scope,
        revision["revision"].as_str().unwrap().to_owned(),
        vec![(key, erp_sdk::Value::Int(100))],
    )
    .await
    .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([bounded_quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview exceeds 100 receipt rows (setting reporting.export_max_rows)."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(
            "DELETE FROM product_product WHERE id=$1",
            &[&applied_origin],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([bounded_quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Access,
        "deleted applied receipt origin"
    );
    assert_eq!(
        error.message(),
        "Issue preview requires readable original cost source records."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let pending_product: i64 = setup
        .query(
            "INSERT INTO product_product(product_tmpl_id) VALUES($1) RETURNING id",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let pending_quant: i64 = setup
        .query("INSERT INTO stock_quant(company_id,product_id,location_id,quantity,reserved_quantity,in_date) VALUES($1,$2,$3,101,0,now()) RETURNING id", &[&company,&pending_product,&place])
        .await.unwrap()[0].get(0);
    let origins: Vec<i64> = setup
        .query(
            "WITH origins AS (
                 INSERT INTO product_product(product_tmpl_id)
                 SELECT $3 FROM generate_series(1,101) RETURNING id
             )
             INSERT INTO stk_value_source(
                 company_id,product_id,origin_model,origin_id,component,source_revision,kind,
                 cost_partition,effective_time,affected_from_time,method,policy,schema_version,
                 semantic_version,request_canonical,request_digest)
             SELECT $1,$2,'product.product',id,'pending-bound',0,'Movement','','2026-01-01',
                    '2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-pending-bound'
             FROM origins RETURNING origin_id",
            &[&company, &pending_product, &template],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    setup.commit().await.unwrap();
    drop(setup);

    let mut tx = runtime.begin_caller(manager).await.unwrap();
    let visible = tx
        .env()
        .records("product.product")
        .unwrap()
        .browse(origins)
        .unwrap()
        .read_rows(&mut tx, &["id"], erp_sdk::Load::Raw)
        .await
        .unwrap();
    assert_eq!(
        visible.len(),
        101,
        "the pending origins are individually readable"
    );
    let record = tx
        .env()
        .records("stock.quant")
        .unwrap()
        .browse([pending_quant])
        .unwrap();
    let error = preview_issue(&record, &mut tx, request("fifo", "1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Issue preview requires readable original cost source records."
    );
    tx.rollback().await.unwrap();
}

async fn verify_pending_preview_origins(
    system: &Orm,
    runtime: &Orm,
    manager: UserId,
    quant: i64,
    company: i64,
    product: i64,
) {
    use serde_json::json;

    let preview = async || {
        let mut tx = runtime.begin_caller(manager).await.unwrap();
        let record = tx
            .env()
            .records("stock.quant")
            .unwrap()
            .browse([quant])
            .unwrap();
        let result = preview_issue(
            &record,
            &mut tx,
            json!({"scope":"primary_unbooked_receipt_projection","method":"fifo","quantity":"6"}),
        )
        .await;
        tx.rollback().await.unwrap();
        result
    };
    let pending_source = "INSERT INTO stk_value_source(
        company_id,product_id,origin_model,origin_id,component,source_revision,kind,
        cost_partition,effective_time,affected_from_time,method,policy,schema_version,
        semantic_version,request_canonical,request_digest)
        VALUES($1,$2,$3,$4,'pending-origin',0,'Movement','','2026-01-01',
               '2026-01-01','fifo','cost_scale_v1',1,1,'{}','preview-pending')
        RETURNING id";

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let readable: i64 = setup
        .query(
            pending_source,
            &[&company, &product, &"product.product", &product],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.commit().await.unwrap();
    drop(setup);

    let error = preview().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Issue preview requires a fully applied receipt cost pool; valuation work is pending."
    );

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let restricted: i64 = setup
        .query(
            pending_source,
            &[&company, &product, &"stock.valuation.source", &readable],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.commit().await.unwrap();
    drop(setup);

    // An origin applies to the product-wide pending check before it has a layer and
    // when its layer belongs to a different lot than the quant's no-lot pool.
    for attach_to_other_lot in [false, true] {
        if attach_to_other_lot {
            let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
            let lot: i64 = setup
                .query(
                    "INSERT INTO stock_lot(name,product_id,company_id)
                     VALUES('Pending preview lot',$1,$2) RETURNING id",
                    &[&product, &company],
                )
                .await
                .unwrap()[0]
                .get(0);
            setup
                .execute(
                    "INSERT INTO stk_cost_layer(company_id,product_id,lot_id,source_id,
                         kind,effective_time,original_quantity,original_value,consumed_quantity,
                         consumed_value,residual_num,residual_den,generation)
                     VALUES($1,$2,$3,$4,'receipt','2026-01-01',1,2,0,0,0,1,1)",
                    &[&company, &product, &lot, &restricted],
                )
                .await
                .unwrap();
            setup.commit().await.unwrap();
            drop(setup);
        }

        let error = preview().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "{}", error.message());
        assert_eq!(
            error.message(),
            "Issue preview requires readable original cost source records."
        );
    }

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    let missing: i64 = setup
        .query(
            pending_source,
            &[&company, &product, &"product.product", &i64::MAX],
        )
        .await
        .unwrap()[0]
        .get(0);
    let apply_sources =
        "INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,applied_mode,
             applied_generation,applied_at,recorded_at)
         SELECT id,company_id,product_id,'applied','inline',1,'2026-01-01','2026-01-01'
         FROM stk_value_source WHERE id=ANY($1)";
    setup
        .execute(apply_sources, &[&vec![readable, restricted]])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);

    // The missing origin is now the only pending work. Its refusal has the same public shape.
    let error = preview().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Issue preview requires readable original cost source records."
    );

    let mut setup = system.begin(SUPERUSER_ID).await.unwrap();
    setup
        .execute(apply_sources, &[&vec![missing]])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);

    let restored = preview().await.unwrap();
    assert_eq!(
        restored["total"],
        json!({"numerator":"66","denominator":"1"})
    );
}

async fn preview_issue(
    record: &erp_sdk::Records,
    tx: &mut erp_sdk::Transaction,
    input: serde_json::Value,
) -> erp_sdk::Result<serde_json::Value> {
    // Same dynamic object shape as the JSON-2 binder.
    let request = erp_sdk::Value::Map(Arc::new(
        input
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                let value = if let Some(id) = value.as_i64() {
                    erp_sdk::Value::Int(id)
                } else {
                    erp_sdk::Value::str(value.as_str().unwrap())
                };
                (key.clone(), value)
            })
            .collect(),
    ));
    let result: erp_sdk::Value = record.call(tx, "preview_issue_cost", (request,)).await?;
    match result {
        erp_sdk::Value::Json(json) => Ok((*json).clone()),
        other => panic!("unexpected preview result {other:?}"),
    }
}

// The SQL-builder expectation belongs to a core consumer, not an addon's dependency graph.
use erp_sdk::__private::TableObjectKind;
use erp_sql::exact_numeric::{carry_check, fraction_check, grid_check};

async fn assert_numeric_domain(
    tx: &mut erp_sdk::Transaction,
    table: &str,
    suffix: &str,
    expected: String,
) {
    let name = format!("{table}_valid_{suffix}");
    let registry = tx.env().registry().clone();
    let model = registry.model_by_table(table).unwrap();
    let object = model.table_object(&name).expect(&name);
    let TableObjectKind::Constraint { definition, .. } = object.kind else {
        panic!("{name} must be a CHECK");
    };
    assert_eq!(definition, expected);
    let rows = tx
        .query(
            "SELECT convalidated FROM pg_constraint WHERE conrelid = to_regclass($1) AND conname::text = $2 AND contype = 'c'",
            &[&table, &name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{name}");
    assert!(rows[0].get::<_, bool>(0), "{name}");
}

#[test]
fn valuation_numeric_domains_match_the_codec() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(numeric_domains());
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn numeric_domains() {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("stock_account").unwrap()],
        )
        .await
        .unwrap();
    assert!(init.post_commit.is_clean());
    assert!(installed.post_commit.is_clean());
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    for (table, column, nullable) in [
        ("stk_value_journal", "grid_delta", false),
        ("stk_value_remainder", "operand_value", true),
        ("stk_cost_state", "grid_value", false),
        ("stk_cost_checkpoint", "grid_value", false),
        ("stk_cost_layer", "original_value", false),
        ("stk_cost_layer", "consumed_value", false),
        ("stk_layer_consumption", "grid_value", false),
    ] {
        assert_numeric_domain(
            &mut tx,
            table,
            column,
            grid_check(column, 18, nullable).unwrap(),
        )
        .await;
    }
    for table in ["stk_cost_state", "stk_cost_checkpoint"] {
        assert_numeric_domain(
            &mut tx,
            table,
            "carry",
            carry_check("currency_carry", "currency_increment").unwrap(),
        )
        .await;
    }
    for (table, numerator, denominator) in [
        ("stk_value_remainder", "remainder_num", "remainder_den"),
        ("stk_cost_layer", "residual_num", "residual_den"),
    ] {
        assert_numeric_domain(
            &mut tx,
            table,
            "fraction",
            fraction_check(numerator, denominator).unwrap(),
        )
        .await;
    }
    tx.rollback().await.unwrap();
}
