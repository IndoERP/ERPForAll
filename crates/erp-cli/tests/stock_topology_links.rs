// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real-install topology properties and default-data lifecycle.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;

use erp_fields::{Cell, ErrorKind};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, Orm, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

#[path = "support/install_defaults.rs"]
mod install_defaults;

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
        ])
        .build()
        .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
fn run(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
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

async fn install(db: &TestDb) -> Orm {
    let system = system(db);
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = system
        .button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("stock").unwrap()],
        )
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    installed.orm
}

async fn xid(tx: &mut erp_orm::Transaction, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name=$1",
        &[&name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

#[test]
fn stock_installs_partner_property_defaults_without_overwriting_customization() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let base = system.initialize(InitOptions::new()).await.unwrap().orm;
        let mut tx = base.begin(SUPERUSER_ID).await.unwrap();
        let company: i64 = tx
            .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let partner = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Existing stock contact".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        drop(tx);
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap()
            .orm;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let customer = xid(&mut tx, "stock_location_customers").await;
        let supplier = xid(&mut tx, "stock_location_suppliers").await;
        let env = tx.env().with_company(company).unwrap();
        let old = env
            .records("res.partner")
            .unwrap()
            .browse([partner])
            .unwrap();
        assert_eq!(
            old.cell(&mut tx, "property_stock_customer").await.unwrap(),
            Cell::Id(customer)
        );
        assert_eq!(
            old.cell(&mut tx, "property_stock_supplier").await.unwrap(),
            Cell::Id(supplier)
        );
        let new = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("New stock contact".into()))],
            )
            .await
            .unwrap();
        assert_eq!(
            new.cell(&mut tx, "property_stock_customer").await.unwrap(),
            Cell::Id(customer)
        );
        for (column, expected) in [
            ("property_stock_customer", customer),
            ("property_stock_supplier", supplier),
        ] {
            let rows = tx.query("SELECT d.json_value FROM ir_default d JOIN ir_model_fields f ON f.id=d.field_id WHERE f.model='res.partner' AND f.name=$1 AND d.company_id IS NULL AND d.user_id IS NULL AND d.condition IS NULL", &[&column]).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get::<_, String>(0), expected.to_string());
        }
        erp_orm::ir_default::set(
            &mut tx,
            &env,
            "res.partner",
            "property_stock_customer",
            &erp_fields::Value::Int(supplier),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        env.records("res.partner")
            .unwrap()
            .call::<_, ()>(&mut tx, "_stock_assign_link_defaults", ())
            .await
            .unwrap();
        assert_eq!(
            old.cell(&mut tx, "property_stock_customer").await.unwrap(),
            Cell::Id(supplier)
        );
        old.write(
            &mut tx,
            RawVals::new().set_cell("property_stock_customer", Cell::Null),
        )
        .await
        .unwrap();
        assert_eq!(
            old.cell(&mut tx, "property_stock_customer").await.unwrap(),
            Cell::Null
        );
        // Malformed external-ID metadata must not become an unrelated numeric location ID.
        let wrong = env
            .records("ir.model.data")
            .unwrap()
            .search(
                &mut tx,
                &erp_sdk::Domain::and([
                    erp_sdk::Domain::condition("module", erp_sdk::Operator::Eq, "stock").unwrap(),
                    erp_sdk::Domain::condition(
                        "name",
                        erp_sdk::Operator::Eq,
                        "stock_location_customers",
                    )
                    .unwrap(),
                ]),
            )
            .await
            .unwrap();
        let env_bad = env.clone();
        let error = tx
            .savepoint(async move |tx| {
                wrong
                    .write(
                        tx,
                        RawVals::new()
                            .set_cell("model", Cell::Str("res.partner".into()))
                            .set_cell("res_id", Cell::Id(partner)),
                    )
                    .await?;
                env_bad
                    .records("res.partner")?
                    .call::<_, ()>(tx, "_stock_assign_link_defaults", ())
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Stock default stock.stock_location_customers must resolve to one stock.location record."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// The fallback rows exist once stock is installed. The routes that create contacts afterwards:
/// a later addon's data file (the Contacts demo, loaded by the System environment of the module
/// loader), a contact and a company made by that System environment (provisioning code), and a
/// contact and a company made by a system administrator in a current company (the form and RPC
/// route, under the shipped access rules). None may fail, and none may store a value that hides
/// the fallback.
#[test]
fn stock_partner_fallbacks_keep_contact_and_company_creates_working() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let initialized = system
            .initialize(InitOptions::new().with_demo(true))
            .await
            .unwrap();
        assert!(initialized.post_commit.is_clean());
        let mut orm = None;
        for module in ["stock", "contacts"] {
            let installed = system
                .button_immediate_install(
                    &system.system_env(Context::new()),
                    &[ModuleName::new(module).unwrap()],
                )
                .await
                .unwrap_or_else(|e| panic!("install {module}: {e:?}"));
            assert!(
                installed.post_commit.is_clean(),
                "{:?}",
                installed.post_commit
            );
            // A failed demo file is rolled back and reported here, not raised.
            assert!(
                installed.data_notices.is_empty(),
                "install {module}: {:?}",
                installed.data_notices
            );
            orm = Some(installed.orm);
        }
        let orm = orm.unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let customers = xid(&mut tx, "stock_location_customers").await;
        let main: i64 = tx
            .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let currency: i64 = tx
            .query(
                "SELECT currency_id::int8 FROM res_company WHERE id=$1",
                &[&main],
            )
            .await
            .unwrap()[0]
            .get(0);
        let demo: i64 = tx
            .query(
                "SELECT res_id::int8 FROM ir_model_data WHERE module='contacts' AND name='partner_demo_lumbung'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let administrators: i64 = tx
            .query(
                "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_system'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let company_vals = |name: &str| {
            RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("currency_id", Cell::Id(currency))
        };
        let contact_vals = |name: &str| RawVals::new().set_cell("name", Cell::Str(name.into()));
        // The System environment names no company, as the module loader's does.
        let system_partner = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(&mut tx, vec![contact_vals("System contact")])
            .await
            .unwrap()
            .ids()[0];
        let provisioned = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(&mut tx, vec![company_vals("Provisioned company")])
            .await
            .unwrap()
            .ids()[0];
        let administrator = create_at(
            &mut tx,
            main,
            "res.users",
            RawVals::new()
                .set_cell("name", Cell::Str("fallback-admin".into()))
                .set_cell("login", Cell::Str("fallback-admin".into()))
                .set_cell("company_id", Cell::Id(main))
                .set_commands(
                    "company_ids",
                    [erp_orm::Command::Set(vec![
                        erp_fields::RecId::from_real(main).unwrap(),
                    ])],
                )
                .set_commands(
                    "group_ids",
                    [erp_orm::Command::Set(vec![
                        erp_fields::RecId::from_real(administrators).unwrap(),
                    ])],
                ),
        )
        .await
        .ids()[0];
        tx.commit().await.unwrap();
        let secured = secured(&orm, &db).await;
        let mut tx = secured
            .begin(erp_orm::UserId::new(administrator).unwrap())
            .await
            .unwrap();
        let admin_partner = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(&mut tx, vec![contact_vals("Administrator contact")])
            .await
            .unwrap()
            .ids()[0];
        let ordinary = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(&mut tx, vec![company_vals("Ordinary company")])
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let mut partners = vec![demo, system_partner, admin_partner];
        for company in [provisioned, ordinary] {
            partners.push(
                tx.query(
                    "SELECT partner_id::int8 FROM res_company WHERE id=$1",
                    &[&company],
                )
                .await
                .unwrap()[0]
                    .get(0),
            );
        }
        for partner in partners {
            // No override stored: every company reads the shared fallback.
            let stored: bool = tx
                .query(
                    "SELECT property_stock_customer IS NULL FROM res_partner WHERE id=$1",
                    &[&partner],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert!(
                stored,
                "partner {partner} stores a customer location override"
            );
            for company in [main, provisioned, ordinary] {
                let record = tx
                    .env()
                    .with_company(company)
                    .unwrap()
                    .records("res.partner")
                    .unwrap()
                    .browse([partner])
                    .unwrap();
                assert_eq!(
                    record
                        .cell(&mut tx, "property_stock_customer")
                        .await
                        .unwrap(),
                    Cell::Id(customers),
                    "partner {partner} in company {company}"
                );
            }
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

fn secured(orm: &Orm, db: &TestDb) -> impl std::future::Future<Output = Orm> {
    let registry = std::sync::Arc::clone(orm.registry());
    let db = db.pool().clone();
    let views = orm
        .env(SUPERUSER_ID, Context::new())
        .generation_attachment::<erp_views::PublishedViewTypes>()
        .unwrap();
    async move {
        let source =
            erp_security::source::PolicySource::build(std::sync::Arc::clone(&registry), db.clone())
                .await
                .unwrap();
        Orm::builder(std::sync::Arc::clone(&registry), db)
            .generation_attachment(erp_orm::GenerationAttachment::new(
                std::sync::Arc::clone(&registry),
                views,
            ))
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
            .policy(std::sync::Arc::new(source) as std::sync::Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap()
    }
}

async fn create_at(
    tx: &mut erp_orm::Transaction,
    company: i64,
    model: &str,
    vals: RawVals,
) -> erp_orm::Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
}

struct InstalledLinks {
    db: TestDb,
    orm: Orm,
    a: i64,
    b: i64,
    c: i64,
    warehouses: Vec<i64>,
    scraps: Vec<i64>,
    users: Vec<i64>,
}
async fn installed_links() -> InstalledLinks {
    let db = TestDb::builder().pool_size(8).create().await.unwrap();
    let orm = install(&db).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let a: i64 = tx
        .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let currency: i64 = tx
        .query(
            "SELECT currency_id::int8 FROM res_company WHERE id=$1",
            &[&a],
        )
        .await
        .unwrap()[0]
        .get(0);
    let b = create_at(
        &mut tx,
        a,
        "res.company",
        RawVals::new()
            .set_cell("name", Cell::Str("Preference company B".into()))
            .set_cell("currency_id", Cell::Id(currency)),
    )
    .await
    .ids()[0];
    let c = create_at(
        &mut tx,
        a,
        "res.company",
        RawVals::new()
            .set_cell("name", Cell::Str("Preference company C".into()))
            .set_cell("currency_id", Cell::Id(currency)),
    )
    .await
    .ids()[0];
    let mut warehouses = Vec::new();
    for (company, code) in [(a, "A"), (b, "B"), (c, "C")] {
        let loc = create_at(
            &mut tx,
            a,
            "stock.location",
            RawVals::new()
                .set_cell("name", Cell::Str(format!("{code} stock").into()))
                .set_cell("company_id", Cell::Id(company)),
        )
        .await
        .ids()[0];
        let w = create_at(
            &mut tx,
            a,
            "stock.warehouse",
            RawVals::new()
                .set_cell("name", Cell::Str(format!("{code} warehouse").into()))
                .set_cell("code", Cell::Str(code.into()))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("view_location_id", Cell::Id(loc))
                .set_cell("lot_stock_id", Cell::Id(loc)),
        )
        .await
        .ids()[0];
        warehouses.push(w);
    }
    let group: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let admin: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let mut users = Vec::new();
    for (login, group) in [
        ("links-admin", admin),
        ("links-owner", group),
        ("links-other", group),
        ("links-single", group),
    ] {
        let u = create_at(
            &mut tx,
            a,
            "res.users",
            RawVals::new()
                .set_cell("name", Cell::Str(login.into()))
                .set_cell("login", Cell::Str(login.into()))
                .set_cell("company_id", Cell::Id(a))
                .set_commands(
                    "company_ids",
                    [erp_orm::Command::Set(if login == "links-single" {
                        vec![erp_fields::RecId::from_real(a).unwrap()]
                    } else {
                        vec![
                            erp_fields::RecId::from_real(a).unwrap(),
                            erp_fields::RecId::from_real(b).unwrap(),
                        ]
                    })],
                )
                .set_commands(
                    "group_ids",
                    [erp_orm::Command::Set(vec![
                        erp_fields::RecId::from_real(group).unwrap(),
                    ])],
                ),
        )
        .await
        .ids()[0];
        users.push(u);
    }
    let mut scraps = Vec::new();
    for company in [a, b] {
        scraps.push(
            create_at(
                &mut tx,
                a,
                "stock.location",
                RawVals::new()
                    .set_cell("name", Cell::Str(format!("Inventory {company}").into()))
                    .set_cell("usage", Cell::Str("inventory".into()))
                    .set_cell("company_id", Cell::Id(company)),
            )
            .await
            .ids()[0],
        );
    }
    // Read grants are confined to this throwaway fixture. Shipped stock ACLs are unchanged.
    for model in ["stock.location", "stock.warehouse"] {
        let id: i64 = tx
            .query("SELECT id::int8 FROM ir_model WHERE model=$1", &[&model])
            .await
            .unwrap()[0]
            .get(0);
        create_at(
            &mut tx,
            a,
            "ir.access",
            RawVals::new()
                .set_cell("name", Cell::Str(format!("Link fixture {model}").into()))
                .set_cell("model_id", Cell::Id(id))
                .set_cell("group_id", Cell::Id(group))
                .set_cell("operation", Cell::Str("r".into())),
        )
        .await;
    }
    tx.install_audit_schema().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = secured(&orm, &db).await;
    InstalledLinks {
        db,
        orm,
        a,
        b,
        c,
        warehouses,
        scraps,
        users,
    }
}

#[test]
fn installed_user_warehouse_preference_respects_self_and_company_bounds() {
    run(async {
        let InstalledLinks {
            db,
            orm,
            a,
            b,
            c,
            warehouses,
            scraps: _,
            users,
        } = installed_links().await;
        let mut tx = orm
            .begin(erp_orm::UserId::new(users[1]).unwrap())
            .await
            .unwrap();
        let owner_a = tx
            .env()
            .with_company(a)
            .unwrap()
            .records("res.users")
            .unwrap()
            .browse([users[1]])
            .unwrap();
        let owner_b = tx
            .env()
            .with_company(b)
            .unwrap()
            .records("res.users")
            .unwrap()
            .browse([users[1]])
            .unwrap();
        owner_a
            .write(
                &mut tx,
                RawVals::new().set_cell("property_warehouse_id", Cell::Id(warehouses[0])),
            )
            .await
            .unwrap();
        owner_b
            .write(
                &mut tx,
                RawVals::new().set_cell("property_warehouse_id", Cell::Id(warehouses[1])),
            )
            .await
            .unwrap();
        assert_eq!(
            owner_a
                .cell(&mut tx, "property_warehouse_id")
                .await
                .unwrap(),
            Cell::Id(warehouses[0])
        );
        assert_eq!(
            owner_b
                .cell(&mut tx, "property_warehouse_id")
                .await
                .unwrap(),
            Cell::Id(warehouses[1])
        );
        let own = owner_a.clone();
        let foreign = warehouses[1];
        let e = tx
            .savepoint(async move |tx| {
                own.write(
                    tx,
                    RawVals::new().set_cell("property_warehouse_id", Cell::Id(foreign)),
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UserError, "{e:?}");
        assert_eq!(
            e.message(),
            "These values link records of different companies:\n- “links-owner” is in company “Main Company, Preference company B”, and “Preferred warehouse” (property_warehouse_id: 'B warehouse') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
        );
        let other = tx
            .env()
            .with_company(a)
            .unwrap()
            .records("res.users")
            .unwrap()
            .browse([users[2]])
            .unwrap();
        let expected_denial = other
            .check_access(&mut tx, erp_orm::Operation::Write)
            .await
            .unwrap_err();
        let e = tx
            .savepoint(async move |tx| {
                other
                    .write(
                        tx,
                        RawVals::new().set_cell("property_warehouse_id", Cell::Id(foreign)),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Access, "{e:?}");
        assert_eq!(e.message(), expected_denial.message());
        let outside_warehouse = warehouses[2];
        let forbidden = tx
            .env()
            .with_company(c)
            .unwrap()
            .records("res.users")
            .unwrap()
            .browse([users[1]])
            .unwrap();
        let expected_company_denial = tx.companies(forbidden.env()).await.unwrap_err();
        let e = tx
            .savepoint(async move |tx| {
                forbidden
                    .write(
                        tx,
                        RawVals::new()
                            .set_cell("property_warehouse_id", Cell::Id(outside_warehouse)),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Access, "{e:?}");
        assert_eq!(e.message(), expected_company_denial.message());
        owner_a
            .write(
                &mut tx,
                RawVals::new().set_cell("property_warehouse_id", Cell::Null),
            )
            .await
            .unwrap();
        assert_eq!(
            owner_a
                .cell(&mut tx, "property_warehouse_id")
                .await
                .unwrap(),
            Cell::Null
        );
        assert_eq!(
            owner_b
                .cell(&mut tx, "property_warehouse_id")
                .await
                .unwrap(),
            Cell::Id(foreign)
        );
        assert_eq!(
            owner_a.cell(&mut tx, "company_id").await.unwrap(),
            Cell::Id(a)
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_uninstall_retires_link_defaults_and_keeps_base_records() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert!(initialized.post_commit.is_clean());
        drop(initialized);
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap()
            .orm;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let count: i64 = tx
            .query("SELECT count(*) FROM res_company", &[])
            .await
            .unwrap()[0]
            .get(0);
        let defaults: Vec<i64> = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND model='ir.default' ORDER BY name", &[]).await.unwrap().iter().map(|row| row.get(0)).collect();
        assert_eq!(defaults.len(), 2);
        tx.rollback().await.unwrap();
        drop(tx);
        let outcome = system
            .button_immediate_uninstall(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            tx.query("SELECT count(*) FROM res_company", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            count
        );
        assert_eq!(
            tx.query(
                "SELECT count(*) FROM ir_default WHERE id=ANY($1)",
                &[&defaults]
            )
            .await
            .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        assert_eq!(tx.query("SELECT count(*) FROM information_schema.columns WHERE table_schema=current_schema() AND ((table_name='res_company' AND column_name='internal_transit_location_id') OR (table_name='res_partner' AND column_name IN ('property_stock_customer','property_stock_supplier')) OR (table_name='res_users' AND column_name='property_warehouse_id'))", &[]).await.unwrap()[0].get::<_, i64>(0), 0);
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

async fn company_lookup_scope(field: &str) {
    let InstalledLinks {
        db,
        orm,
        a,
        b,
        warehouses,
        scraps,
        users,
        ..
    } = installed_links().await;
    let mut tx = orm
        .begin(erp_orm::UserId::new(users[1]).unwrap())
        .await
        .unwrap();
    let env = tx.env().with_context([(
        "allowed_company_ids",
        erp_fields::Value::list([erp_fields::Value::Int(a), erp_fields::Value::Int(b)]),
    )]);
    let expected = if field == "scrap_location_id" {
        scraps.iter().copied().map(Cell::Id).collect::<Vec<_>>()
    } else {
        env.records("stock.warehouse")
            .unwrap()
            .browse(warehouses[..2].iter().copied())
            .unwrap()
            .cells(&mut tx, "lot_stock_id")
            .await
            .unwrap()
    };
    let companies = env.records("res.company").unwrap().browse([a, b]).unwrap();
    assert_eq!(companies.cells(&mut tx, field).await.unwrap(), expected);
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = orm
        .begin(erp_orm::UserId::new(users[3]).unwrap())
        .await
        .unwrap();
    let own = tx
        .env()
        .with_company(a)
        .unwrap()
        .records("res.company")
        .unwrap()
        .browse([a])
        .unwrap();
    assert_eq!(own.cell(&mut tx, field).await.unwrap(), expected[0]);
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

#[test]
fn installed_scrap_lookup_uses_the_record_company() {
    run(company_lookup_scope("scrap_location_id"));
}

#[test]
fn installed_stock_lookup_uses_the_record_company() {
    run(company_lookup_scope("default_stock_location_id"));
}
