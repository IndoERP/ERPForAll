// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock security through the real installer, compiled policy and ordinary principal resolver.

use super::*;
use std::collections::BTreeSet;
use std::sync::Arc;

use erp_fields::RecId;
use erp_orm::{Command, Operation, UserId};
use erp_security::{resolver::Resolver, source::PolicySource};

const OPS: [(Operation, char); 4] = [
    (Operation::Create, 'c'),
    (Operation::Read, 'r'),
    (Operation::Write, 'u'),
    (Operation::Unlink, 'd'),
];

// An independent behavioral oracle: employee / operator / manager, including inherited grants.
const STOCK_MATRIX: &[(&str, [&str; 3])] = &[
    ("product.removal", ["r", "r", "r"]),
    ("stock.location", ["r", "r", "crud"]),
    ("stock.location.policy", ["r", "r", "crud"]),
    ("stock.lot", ["", "crud", "crud"]),
    ("stock.lot.name.scope", ["", "", ""]),
    ("stock.move", ["", "cru", "crud"]),
    ("stock.move.line", ["", "crud", "crud"]),
    // L6': corrections are written by the correction engine only; serial positions by the engine
    ("stock.move.line.correction", ["", "r", "r"]),
    ("stock.move.line.serial", ["", "crud", "crud"]),
    ("stock.package", ["", "", ""]), // supplies company scope before grants.
    ("stock.package.type", ["", "r", "crud"]),
    ("stock.picking", ["", "crud", "crud"]),
    ("stock.picking.type", ["r", "r", "crud"]),
    ("stock.putaway.rule", ["r", "r", "crud"]),
    ("stock.quant", ["r", "cru", "cru"]),
    ("stock.reference", ["cru", "cru", "cru"]),
    ("stock.route", ["r", "r", "crud"]),
    ("stock.rule", ["r", "r", "crud"]),
    ("stock.serial.position", ["r", "r", "r"]),
    ("stock.storage.category", ["r", "r", "crud"]),
    ("stock.storage.category.capacity", ["r", "r", "crud"]),
    ("stock.topology.version", ["", "", ""]),
    ("stock.warehouse", ["r", "r", "crud"]),
    ("stock.warehouse.orderpoint", ["", "r", "crud"]),
];

const LIGHT: &[&str] = &[
    "group_production_lot",
    "group_adv_location",
    "group_stock_multi_locations",
    "group_stock_picking_batch",
    "group_tracking_lot",
    "group_tracking_owner",
    "group_lot_on_delivery_slip",
    "group_stock_lot_print_gs1",
    "group_stock_sign_delivery",
    "group_warning_stock",
];

const CROSS_MATRIX: &[(&str, [&str; 3])] = &[
    ("product.product", ["r", "r", "r"]),
    ("product.template", ["r", "r", "r"]),
    ("product.pricelist", ["r", "r", "crud"]),
    ("product.attribute", ["r", "r", "crud"]),
    ("product.attribute.value", ["r", "r", "crud"]),
    ("res.partner", ["r", "r", "cru"]),
    ("barcode.nomenclature", ["r", "r", "crud"]),
    ("barcode.rule", ["r", "r", "crud"]),
];

pub(super) async fn xid(orm: &Orm, module: &str, name: &str) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module=$1 AND name=$2",
            &[&module, &name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "missing or duplicate {module}.{name}");
    let id = rows[0].get(0);
    tx.rollback().await.unwrap();
    id
}

pub(super) async fn secure(db: &TestDb, installed: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
        .await
        .unwrap();
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.registry()),
            installed
                .env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(Resolver::for_registry(installed.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

async fn user(tx: &mut erp_orm::Transaction, login: &str, company: i64, group: i64) -> i64 {
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(login.into()))
                    .set_cell("login", Cell::Str(login.into()))
                    .set_cell(
                        "password",
                        Cell::Str(format!("{login}-fixture-password").into()),
                    )
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(company).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

pub(super) async fn roles(orm: &Orm) -> Vec<i64> {
    let company = xid(orm, "base", "main_company").await;
    let mut groups = Vec::new();
    for (module, name) in [
        ("base", "group_system"),
        ("base", "group_user"),
        ("stock", "group_stock_user"),
        ("stock", "group_stock_manager"),
        ("base", "group_portal"),
        ("base", "group_public"),
    ] {
        groups.push(xid(orm, module, name).await);
    }
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let mut users = Vec::new();
    for (i, group) in groups.into_iter().enumerate() {
        let id = user(&mut tx, &format!("stock-security-{i}"), company, group).await;
        assert_ne!(UserId::new(id).unwrap(), SUPERUSER_ID);
        users.push(id);
    }
    tx.install_audit_schema().await.unwrap();
    tx.commit().await.unwrap();
    users
}

async fn model_denial_message(orm: &Orm, model: &str, op: Operation) -> String {
    let (letter, verb) = match op {
        Operation::Read => ("r", "access"),
        Operation::Create => ("c", "create"),
        Operation::Write => ("u", "modify"),
        Operation::Unlink => ("d", "delete"),
    };
    let description = &orm.registry().model(model).unwrap().description;
    super::stock_access::refusal(orm, verb, letter, description, model).await
}

async fn assert_model_denial(orm: &Orm, model: &str, op: Operation, error: &erp_sdk::Error) {
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), model_denial_message(orm, model, op).await);
}

#[test]
fn stock_security_groups_and_light_classification() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        assert_eq!(
            strings(
                &installed,
                "SELECT p.name || '/' || g.sequence::text || '/' || d.name FROM res_groups g
             JOIN ir_model_data d ON d.model='res.groups' AND d.res_id=g.id
             JOIN res_groups_privilege p ON p.id=g.privilege_id
             WHERE d.module='stock' ORDER BY g.sequence"
            )
            .await,
            [
                "Inventory/10/group_stock_user",
                "Inventory/20/group_stock_manager"
            ]
        );
        let users = roles(&installed).await;
        let orm = secure(&db, &installed).await;
        for (id, expected) in [(users[0], true), (users[2], false), (users[3], true)] {
            let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
            let env = tx.env();
            assert!(tx.has_group(&env, "base.group_user").await.unwrap());
            assert!(tx.has_group(&env, "stock.group_stock_user").await.unwrap());
            assert_eq!(
                tx.has_group(&env, "stock.group_stock_manager")
                    .await
                    .unwrap(),
                expected
            );
            tx.rollback().await.unwrap();
        }
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let groups = tx.env().records("res.groups").unwrap();
        let light: Vec<String> = groups
            .call(&mut tx, "_get_light_group_xmlids", ())
            .await
            .unwrap();
        for name in LIGHT {
            let xmlid = format!("stock.{name}");
            assert_eq!(
                light.iter().filter(|id| **id == xmlid).count(),
                1,
                "{xmlid}"
            );
            let id = xid(&installed, "stock", name).await;
            assert!(
                groups
                    .browse([id])
                    .unwrap()
                    .call::<(), bool>(&mut tx, "_is_light_groups", ())
                    .await
                    .unwrap()
            );
        }
        for xmlid in [
            "base.group_user",
            "uom.group_uom",
            "product.group_product_variant",
        ] {
            assert_eq!(
                light.iter().filter(|id| id.as_str() == xmlid).count(),
                1,
                "{xmlid}"
            );
        }
        let multi = xid(&installed, "stock", "group_stock_multi_warehouses").await;
        assert!(
            !groups
                .browse([multi])
                .unwrap()
                .call::<(), bool>(&mut tx, "_is_light_groups", ())
                .await
                .unwrap()
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_installed_role_matrix_and_model_coverage() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let orm = secure(&db, &installed).await;
        let registered = strings(&installed,
            "SELECT DISTINCT m.model FROM ir_model m JOIN ir_model_data d ON d.model='ir.model' AND d.res_id=m.id
             WHERE d.module='stock' ORDER BY m.model").await.into_iter().collect::<BTreeSet<_>>();
        let covered = STOCK_MATRIX
            .iter()
            .map(|(model, _)| model.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered, covered,
            "every concrete stock model needs a grant or explicit system-only/deferred decision"
        );
        for (role, id) in users[1..4].iter().enumerate() {
            let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
            for (name, rights) in STOCK_MATRIX.iter().chain(CROSS_MATRIX) {
                let model = tx.env().records(name).unwrap();
                for (op, letter) in OPS {
                    assert_eq!(
                        model.has_access(&mut tx, op).await.unwrap(),
                        rights[role].contains(letter),
                        "role {role} {name} {op:?}"
                    );
                }
            }
            tx.rollback().await.unwrap();
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_orm_denials_and_quant_guard_survive_grants() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let orm = secure(&db, &installed).await;
        let mut tx = orm.begin(UserId::new(users[2]).unwrap()).await.unwrap();
        let error = tx
            .env()
            .records("product.pricelist")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Forbidden pricelist".into()))],
            )
            .await
            .unwrap_err();
        assert_model_denial(&orm, "product.pricelist", Operation::Create, &error).await;
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(users[1]).unwrap()).await.unwrap();
        for model in ["stock.picking", "stock.move", "stock.move.line"] {
            let error = tx
                .env()
                .records(model)
                .unwrap()
                .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
                .await
                .unwrap_err();
            assert_model_denial(&orm, model, Operation::Read, &error).await;
        }
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(users[2]).unwrap()).await.unwrap();
        let error = tx
            .env()
            .records("stock.quant")
            .unwrap()
            .create(&mut tx, vec![RawVals::new()])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "Stock quants cannot be created directly: balances change only through stock moves and inventory adjustments."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_upgrade_preserves_identity_and_group_customization() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert!(initialized.post_commit.is_clean());
        let install = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(install.post_commit.is_clean());
        let installed = install.orm;
        let identity_sql = "SELECT name || '/' || model || '/' || res_id::text FROM ir_model_data
            WHERE module='stock' AND model IN ('res.groups', 'res.groups.privilege', 'ir.access') ORDER BY name";
        let before = strings(&installed, identity_sql).await;
        let manager = xid(&installed, "stock", "group_stock_manager").await;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([manager])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("name", Cell::Str("Local stock administrator".into())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        for _ in 0..2 {
            let upgraded = system
                .button_immediate_upgrade(
                    &system.system_env(Context::new()),
                    &[ModuleName::new("stock").unwrap()],
                )
                .await
                .unwrap();
            assert!(
                upgraded.post_commit.is_clean(),
                "{:?}",
                upgraded.post_commit
            );
            assert_eq!(strings(&upgraded.orm, identity_sql).await, before);
            assert_eq!(
                xid(&upgraded.orm, "stock", "group_stock_manager").await,
                manager
            );
            let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
            assert_eq!(
                tx.env()
                    .records("res.groups")
                    .unwrap()
                    .browse([manager])
                    .unwrap()
                    .cell(&mut tx, "name")
                    .await
                    .unwrap(),
                Cell::Str("Local stock administrator".into())
            );
            let light: Vec<String> = tx
                .env()
                .records("res.groups")
                .unwrap()
                .call(&mut tx, "_get_light_group_xmlids", ())
                .await
                .unwrap();
            for name in LIGHT {
                assert_eq!(
                    light
                        .iter()
                        .filter(|id| **id == format!("stock.{name}"))
                        .count(),
                    1
                );
            }
            tx.rollback().await.unwrap();
        }
        db.cleanup().await.unwrap();
    });
}

/// C1 of the lead ruling: absence of permission never grants read or write, even to Settings.
#[test]
fn stock_security_no_permission_rows_deny_every_ordinary_role() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let orm = secure(&db, &installed).await;
        for id in users {
            let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
            for name in [
                "stock.lot.name.scope",
                "stock.topology.version",
                "stock.package",
            ] {
                let records = tx.env().records(name).unwrap();
                let specification = orm.registry().model(name).unwrap();
                for op in [Operation::Read, Operation::Write] {
                    let error = records.check_access(&mut tx, op).await.unwrap_err();
                    assert_eq!(error.kind(), ErrorKind::Access);
                    assert_eq!(
                        error.message(),
                        erp_security::explain::model_message(specification, op, &[])
                    );
                }
            }
            tx.rollback().await.unwrap();
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_generated_company_rules_scope_real_records() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let a = xid(&installed, "base", "main_company").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let companies = setup
            .env()
            .with_company(a)
            .unwrap()
            .records("res.company")
            .unwrap();
        let currency = companies
            .browse([a])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let b = companies
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Stock security other company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut fixtures = Vec::new();
        for model in ["stock.location", "stock.route"] {
            let mut ids = Vec::new();
            for (label, company) in [
                ("Own", Cell::Id(a)),
                ("Foreign", Cell::Id(b)),
                ("Shared", Cell::Null),
            ] {
                ids.push(
                    setup
                        .env()
                        .records(model)
                        .unwrap()
                        .create(
                            &mut setup,
                            vec![
                                RawVals::new()
                                    .set_cell("name", Cell::Str(format!("Security {label}").into()))
                                    .set_cell("company_id", company),
                            ],
                        )
                        .await
                        .unwrap()
                        .ids()[0],
                );
            }
            fixtures.push((model, ids));
        }
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        for id in &users[1..4] {
            let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
            for (model, ids) in &fixtures {
                for (i, record_id) in ids.iter().enumerate() {
                    let record = tx
                        .env()
                        .records(model)
                        .unwrap()
                        .browse([*record_id])
                        .unwrap();
                    assert_eq!(
                        record.has_access(&mut tx, Operation::Read).await.unwrap(),
                        i != 1,
                        "{model} row {i}"
                    );
                }
            }
            let location_ids = &fixtures[0].1;
            let policy = tx.query("SELECT id, location_id FROM stock_location_policy WHERE location_id=ANY($1) ORDER BY location_id", &[location_ids]).await.unwrap();
            assert_eq!(policy.len(), 3);
            for row in policy {
                let record_id: i64 = row.get(0);
                let location: i64 = row.get(1);
                let record = tx
                    .env()
                    .records("stock.location.policy")
                    .unwrap()
                    .browse([record_id])
                    .unwrap();
                assert_eq!(
                    record.has_access(&mut tx, Operation::Read).await.unwrap(),
                    location != location_ids[1]
                );
            }
            tx.rollback().await.unwrap();
        }
        // A manager's grant cannot write a row outside the active company.
        let mut tx = orm.begin(UserId::new(users[3]).unwrap()).await.unwrap();
        let foreign = tx
            .env()
            .records("stock.location")
            .unwrap()
            .browse([fixtures[0].1[1]])
            .unwrap();
        assert!(!foreign.has_access(&mut tx, Operation::Write).await.unwrap());
        let error = foreign
            .write(
                &mut tx,
                RawVals::new().set_cell("name", Cell::Str("Forbidden".into())),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        // Independent fixture evidence: the manager belongs only to A, and this row belongs
        // to B. The installed generated restriction must exist; core owns its diagnostic text.
        let rows = tx.query(
            "SELECT a.id, a.name FROM ir_model m JOIN ir_access a ON a.model_id=m.id AND a.generated WHERE m.model='stock.location'",
            &[],
        ).await.unwrap();
        assert_eq!(rows.len(), 1);
        let rule_id: i64 = rows[0].get(0);
        let rule_name: String = rows[0].get(1);
        let evidence = erp_orm::RecordEvidence::new(
            vec![
                erp_orm::EvidenceColumn::new(
                    erp_orm::RuleRef::new(rule_id, rule_name).unwrap(),
                    erp_orm::RuleKind::Restriction,
                    erp_sdk::Domain::constant(false),
                )
                .with_mentions_company(true),
            ],
            vec![
                erp_orm::EvidenceRow::new(fixtures[0].1[1], None, vec![false]).with_company(Some(
                    erp_orm::EvidenceCompany::foreign(erp_orm::CompanyId::new(b).unwrap()),
                )),
            ],
            "stock-security-3",
        )
        .unwrap();
        assert_eq!(
            error.message(),
            erp_security::explain::RecordMessage::new(
                orm.registry().model("stock.location").unwrap(),
                Operation::Write,
                users[3],
                &evidence,
            )
            .render()
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_route_changes_never_skip_records_of_hidden_companies() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let a = xid(&installed, "base", "main_company").await;
        let manager = xid(&installed, "stock", "group_stock_manager").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let companies = setup
            .env()
            .with_company(a)
            .unwrap()
            .records("res.company")
            .unwrap();
        let currency = companies
            .browse([a])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let b = companies
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Route security other company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let both = create(
            &mut setup,
            "res.users",
            RawVals::new()
                .set_cell("name", Cell::Str("route-security-both".into()))
                .set_cell("login", Cell::Str("route-security-both".into()))
                .set_cell(
                    "password",
                    Cell::Str("route-security-both-fixture-password".into()),
                )
                .set_cell("company_id", Cell::Id(a))
                .set_commands(
                    "company_ids",
                    [Command::set([
                        RecId::from_real(a).unwrap(),
                        RecId::from_real(b).unwrap(),
                    ])],
                )
                .set_commands(
                    "group_ids",
                    [Command::set([RecId::from_real(manager).unwrap()])],
                ),
        )
        .await;
        let route = create(
            &mut setup,
            "stock.route",
            RawVals::new()
                .set_cell("name", Cell::Str("Route security shared".into()))
                .set_cell("warehouse_selectable", Cell::Bool(true)),
        )
        .await;
        let mut rules = Vec::new();
        let mut warehouses = Vec::new();
        for (label, company) in [("A", a), ("B", b)] {
            let root = create(
                &mut setup,
                "stock.location",
                RawVals::new()
                    .set_cell("name", Cell::Str(format!("Route security {label}").into()))
                    .set_cell("usage", Cell::Str("view".into())),
            )
            .await;
            warehouses.push(
                create(
                    &mut setup,
                    "stock.warehouse",
                    RawVals::new()
                        .set_cell("name", Cell::Str(format!("Route security {label}").into()))
                        .set_cell("code", Cell::Str(format!("RS{label}").into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("view_location_id", Cell::Id(root))
                        .set_cell("lot_stock_id", Cell::Id(root)),
                )
                .await,
            );
            let operation = create(
                &mut setup,
                "stock.picking.type",
                RawVals::new()
                    .set_cell("name", Cell::Str(format!("Route security {label}").into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("default_location_src_id", Cell::Id(root))
                    .set_cell("default_location_dest_id", Cell::Id(root)),
            )
            .await;
            rules.push(
                create(
                    &mut setup,
                    "stock.rule",
                    RawVals::new()
                        .set_cell("name", Cell::Str(format!("Route security {label}").into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("route_id", Cell::Id(route))
                        .set_cell("location_dest_id", Cell::Id(root))
                        .set_cell("picking_type_id", Cell::Id(operation)),
                )
                .await,
            );
        }
        // Company consistency refuses links from a shared route to company warehouses, so
        // switching warehouse selection off never has a link of a hidden company to keep.
        let shared_route = setup
            .env()
            .records("stock.route")
            .unwrap()
            .browse([route])
            .unwrap();
        let error = setup
            .savepoint(async |tx| {
                shared_route
                    .write(
                        tx,
                        RawVals::new().set_commands(
                            "warehouse_ids",
                            [Command::set(
                                warehouses.iter().map(|w| RecId::from_real(*w).unwrap()),
                            )],
                        ),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "These values link records of different companies:\n- “Route security shared” is in company “”, and “Warehouses” (warehouse_ids: 'Route security A', 'Route security B') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
        );
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        let state = async |tx: &mut erp_orm::Transaction| {
            let route_row = tx
                .query("SELECT active FROM stock_route WHERE id = $1", &[&route])
                .await
                .unwrap();
            let rule_rows = tx
                .query(
                    "SELECT active FROM stock_rule WHERE id = ANY($1) ORDER BY id",
                    &[&rules],
                )
                .await
                .unwrap();
            (
                route_row[0].get::<_, bool>(0),
                rule_rows
                    .iter()
                    .map(|r| r.get::<_, bool>(0))
                    .collect::<Vec<_>>(),
            )
        };
        // A manager of company A only: refused by name, and nothing changed.
        let mut tx = orm.begin(UserId::new(users[3]).unwrap()).await.unwrap();
        let shared = tx
            .env()
            .records("stock.route")
            .unwrap()
            .browse([route])
            .unwrap();
        let result = tx
            .savepoint(async |tx| {
                shared
                    .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                    .await
            })
            .await
            .map_err(|e| (e.kind(), e.message().to_owned()));
        assert_eq!(
            (result, state(&mut tx).await),
            (
                Err((
                    ErrorKind::Access,
                    "This route has rules of companies you cannot access, so it cannot be archived or restored here. Ask a user with access to all of its companies to do it.".to_owned()
                )),
                (true, vec![true, true])
            )
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // A manager of both companies: the archive and restore reach every rule.
        let mut tx = orm.begin(UserId::new(both).unwrap()).await.unwrap();
        let shared = tx
            .env()
            .records("stock.route")
            .unwrap()
            .browse([route])
            .unwrap()
            .with_context([(
                "allowed_company_ids",
                erp_fields::Value::list([erp_fields::Value::Int(a), erp_fields::Value::Int(b)]),
            )]);
        for active in [false, true] {
            shared
                .write(
                    &mut tx,
                    RawVals::new().set_cell("active", Cell::Bool(active)),
                )
                .await
                .unwrap();
            assert_eq!(state(&mut tx).await, (active, vec![active, active]));
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

async fn create(tx: &mut erp_orm::Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

fn operation_vals(company: i64, product: i64, unit: i64, location: i64) -> RawVals {
    RawVals::new()
        .set_cell("company_id", Cell::Id(company))
        .set_cell("product_id", Cell::Id(product))
        .set_cell("uom_id", Cell::Id(unit))
        .set_cell("location_id", Cell::Id(location))
        .set_cell("location_dest_id", Cell::Id(location))
        .set_cell("quantity_product_uom", Cell::Decimal(Decimal::ZERO))
        .set_cell("uom_factor", Cell::Decimal(Decimal::ONE))
        .set_cell("product_uom_factor", Cell::Decimal(Decimal::ONE))
        .set_cell("product_unit_digits", Cell::Int(2))
}

#[test]
fn stock_security_transactional_company_and_line_mutations() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let a = xid(&installed, "base", "main_company").await;
        let unit = xid(&installed, "uom", "product_uom_unit").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let currency = setup
            .env()
            .records("res.company")
            .unwrap()
            .browse([a])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let b = setup
            .env()
            .with_company(a)
            .unwrap()
            .records("res.company")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Foreign transaction company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let product = create(
            &mut setup,
            "product.product",
            RawVals::new().set_cell("name", Cell::Str("Security widget".into())),
        )
        .await;
        let location = create(
            &mut setup,
            "stock.location",
            RawVals::new().set_cell("name", Cell::Str("Shared security stock".into())),
        )
        .await;
        let mut fixtures = Vec::new();
        for company in [a, b] {
            let vals = operation_vals(company, product, unit, location);
            let mv = create(
                &mut setup,
                "stock.move",
                vals.clone()
                    .set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
            )
            .await;
            let line = create(
                &mut setup,
                "stock.move.line",
                vals.set_cell("move_id", Cell::Id(mv))
                    .set_cell("child_mode", Cell::Bool(true)),
            )
            .await;
            let child = create(
                &mut setup,
                "stock.move.line.serial",
                RawVals::new()
                    .set_cell("line_id", Cell::Id(line))
                    .set_cell("move_id", Cell::Id(mv)),
            )
            .await;
            // Fixture only: quants are engine-owned and deliberately cannot be created by ORM.
            let quant = setup.query("INSERT INTO stock_quant (company_id, product_id, location_id, quantity, reserved_quantity, in_date)
                VALUES ($1,$2,$3,1,0,now()) RETURNING id", &[&company, &product, &location]).await.unwrap()[0].get::<_, i64>(0);
            fixtures.push((mv, line, child, quant));
        }
        for table in ["stock_quant", "stock_move_line"] {
            let rows = setup.query("SELECT is_nullable FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1 AND column_name='company_id'", &[&table]).await.unwrap();
            assert_eq!(
                rows[0].get::<_, String>(0),
                "NO",
                "{table}: NULL company must be refused by storage"
            );
        }
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        for (role, id) in users[1..4].iter().enumerate() {
            let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
            for (i, &(mv, line, child, quant)) in fixtures.iter().enumerate() {
                for (model, row, field) in [
                    ("stock.move", mv, "sequence"),
                    ("stock.move.line", line, "active"),
                    ("stock.move.line.serial", child, "active"),
                    ("stock.quant", quant, "quantity"),
                ] {
                    let record = tx.env().records(model).unwrap().browse([row]).unwrap();
                    let allowed = i == 0 && (role > 0 || model == "stock.quant");
                    assert_eq!(
                        record.has_access(&mut tx, Operation::Read).await.unwrap(),
                        allowed,
                        "{model}, role {role}, company index {i}"
                    );
                    if allowed {
                        record.cell(&mut tx, field).await.unwrap();
                    }
                }
            }
            tx.rollback().await.unwrap();
        }
        // Operators edit and remove their own open line although they cannot delete the move.
        let mut tx = orm.begin(UserId::new(users[2]).unwrap()).await.unwrap();
        let (mv, line, _, _) = fixtures[0];
        assert!(
            !tx.env()
                .records("stock.move")
                .unwrap()
                .browse([mv])
                .unwrap()
                .has_access(&mut tx, Operation::Unlink)
                .await
                .unwrap()
        );
        let record = tx
            .env()
            .records("stock.move.line")
            .unwrap()
            .browse([line])
            .unwrap();
        record
            .write(&mut tx, RawVals::new().set_cell("picked", Cell::Bool(true)))
            .await
            .unwrap();
        assert_eq!(
            record.cell(&mut tx, "picked").await.unwrap(),
            Cell::Bool(true)
        );
        record.unlink(&mut tx).await.unwrap();
        assert_eq!(
            tx.query("SELECT id FROM stock_move_line WHERE id=$1", &[&line])
                .await
                .unwrap()
                .len(),
            0
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_security_engine_owned_rows_and_columns_refuse_generic_writes() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let company = xid(&installed, "base", "main_company").await;
        let unit = xid(&installed, "uom", "product_uom_unit").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let product = create(
            &mut setup,
            "product.product",
            RawVals::new().set_cell("name", Cell::Str("Engine column widget".into())),
        )
        .await;
        let location = create(
            &mut setup,
            "stock.location",
            RawVals::new().set_cell("name", Cell::Str("Engine column stock".into())),
        )
        .await;
        let vals = operation_vals(company, product, unit, location);
        let mv = create(
            &mut setup,
            "stock.move",
            vals.clone()
                .set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
        )
        .await;
        let line = create(
            &mut setup,
            "stock.move.line",
            vals.clone().set_cell("move_id", Cell::Id(mv)),
        )
        .await;
        // Fixture only: correction rows come from the correction engine, never from the ORM.
        let correction = setup
            .query(
                "INSERT INTO stock_move_line_correction (company_id, product_id, move_line_id, seq, kind, effective_at, recorded_at)
                 VALUES ($1,$2,$3,1,'edit',(now() AT TIME ZONE 'UTC'),(now() AT TIME ZONE 'UTC')) RETURNING id",
                &[&company, &product, &line],
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        let stamp = Cell::DateTime(erp_fields::DateTime::now());
        let engine_columns: [(&str, &'static str, &'static str, Cell); 4] = [
            ("stock.move", "stock move", "done_at", stamp.clone()),
            ("stock.move.line", "stock move line", "done_at", stamp),
            (
                "stock.move.line",
                "stock move line",
                "membership",
                Cell::Str("done".into()),
            ),
            (
                "stock.move.line",
                "stock move line",
                "correction_seq",
                Cell::Int(1),
            ),
        ];
        for id in [users[2], users[3]] {
            let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
            let corrections = tx.env().records("stock.move.line.correction").unwrap();
            corrections
                .browse([correction])
                .unwrap()
                .cell(&mut tx, "seq")
                .await
                .unwrap();
            let error = corrections
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("company_id", Cell::Id(company))
                            .set_cell("product_id", Cell::Id(product))
                            .set_cell("move_line_id", Cell::Id(line))
                            .set_cell("seq", Cell::Int(2))
                            .set_cell("kind", Cell::Str("edit".into())),
                    ],
                )
                .await
                .unwrap_err();
            assert_model_denial(
                &orm,
                "stock.move.line.correction",
                Operation::Create,
                &error,
            )
            .await;
            let record = corrections.browse([correction]).unwrap();
            let error = record
                .write(&mut tx, RawVals::new().set_cell("seq", Cell::Int(3)))
                .await
                .unwrap_err();
            assert_model_denial(&orm, "stock.move.line.correction", Operation::Write, &error).await;
            let error = record.unlink(&mut tx).await.unwrap_err();
            assert_model_denial(
                &orm,
                "stock.move.line.correction",
                Operation::Unlink,
                &error,
            )
            .await;
            for (model, what, field, value) in engine_columns.iter().cloned() {
                let label = orm
                    .registry()
                    .model(model)
                    .unwrap()
                    .field(field)
                    .unwrap()
                    .string
                    .clone();
                let expected = erp_addon_stock::StockError::EngineOnlyField {
                    what,
                    field: label.to_string(),
                }
                .to_string();
                assert!(expected.starts_with(&*label), "{expected}");
                let row = if model == "stock.move" { mv } else { line };
                let error = tx
                    .env()
                    .records(model)
                    .unwrap()
                    .browse([row])
                    .unwrap()
                    .write(&mut tx, RawVals::new().set_cell(field, value.clone()))
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), ErrorKind::UserError, "write {model}.{field}");
                assert_eq!(error.message(), expected, "write {model}.{field}");
                let mut create_vals = vals.clone().set_cell(field, value.clone());
                if model == "stock.move.line" {
                    create_vals = create_vals.set_cell("move_id", Cell::Id(mv));
                } else {
                    create_vals = create_vals.set_cell("product_qty", Cell::Decimal(Decimal::ZERO));
                }
                let error = tx
                    .env()
                    .records(model)
                    .unwrap()
                    .create(&mut tx, vec![create_vals])
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), ErrorKind::UserError, "create {model}.{field}");
                assert_eq!(error.message(), expected, "create {model}.{field}");
            }
            // the columns the engine does not own stay writable for the same callers
            tx.env()
                .records("stock.move.line")
                .unwrap()
                .browse([line])
                .unwrap()
                .write(&mut tx, RawVals::new().set_cell("picked", Cell::Bool(true)))
                .await
                .unwrap();
            tx.rollback().await.unwrap();
        }
        db.cleanup().await.unwrap();
    });
}

/// The JSON-2 values of a move (`line_of` set: of a line of that move) built from `vals`'s scalars.
fn json_vals(vals: &RawVals, line_of: Option<i64>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, cell) in vals.iter() {
        let value = match cell {
            Cell::Id(id) => serde_json::json!(id),
            Cell::Int(n) => serde_json::json!(n),
            Cell::Decimal(d) => serde_json::json!(d.to_string().parse::<f64>().unwrap()),
            other => panic!("json_vals: unexpected cell {other:?}"),
        };
        map.insert(name.to_owned(), value);
    }
    match line_of {
        Some(mv) => map.insert("move_id".into(), serde_json::json!(mv)),
        None => map.insert("product_qty".into(), serde_json::json!(0)),
    };
    serde_json::Value::Object(map)
}

/// key on authenticated JSON-2 and typed create, a nested create through a transfer, and an
/// `ir.default` row never reaches them)
#[test]
fn stock_security_engine_owned_columns_refuse_context_and_stored_defaults() {
    run(async {
        use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let company = xid(&installed, "base", "main_company").await;
        let unit = xid(&installed, "uom", "product_uom_unit").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let product = create(
            &mut setup,
            "product.product",
            RawVals::new().set_cell("name", Cell::Str("Default column widget".into())),
        )
        .await;
        let location = create(
            &mut setup,
            "stock.location",
            RawVals::new().set_cell("name", Cell::Str("Default column stock".into())),
        )
        .await;
        let vals = operation_vals(company, product, unit, location);
        let mv = create(
            &mut setup,
            "stock.move",
            vals.clone()
                .set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
        )
        .await;
        let picking_type = create(
            &mut setup,
            "stock.picking.type",
            RawVals::new()
                .set_cell("name", Cell::Str("Default column receipts".into()))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("default_location_src_id", Cell::Id(location))
                .set_cell("default_location_dest_id", Cell::Id(location)),
        )
        .await;
        let picking = create(
            &mut setup,
            "stock.picking",
            RawVals::new()
                .set_cell("name", Cell::Str("Default column transfer".into()))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("picking_type_id", Cell::Id(picking_type))
                .set_cell("location_id", Cell::Id(location))
                .set_cell("location_dest_id", Cell::Id(location)),
        )
        .await;
        // A stored default (as an administrator would set it) for an engine-owned column.
        let env = setup.env();
        erp_orm::ir_default::set(
            &mut setup,
            &env,
            "stock.move.line",
            "membership",
            &erp_fields::Value::str("done"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        let auth = Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap();
        let service = erp_rpc::RpcService::new(orm.clone(), Host, erp_rpc::RpcConfig::default());
        let stamp = "2026-01-02 03:04:05";
        let defaults: [(&str, &'static str, &str, serde_json::Value); 5] = [
            (
                "stock.move",
                "stock move",
                "done_at",
                serde_json::json!(stamp),
            ),
            (
                "stock.move",
                "stock move",
                "open_line_version",
                serde_json::json!(7),
            ),
            (
                "stock.move.line",
                "stock move line",
                "done_at",
                serde_json::json!(stamp),
            ),
            (
                "stock.move.line",
                "stock move line",
                "membership",
                serde_json::json!("done"),
            ),
            (
                "stock.move.line",
                "stock move line",
                "correction_seq",
                serde_json::json!(3),
            ),
        ];
        let expected = |model: &str, what: &'static str, field: &str| {
            let label = orm
                .registry()
                .model(model)
                .unwrap()
                .field(field)
                .unwrap()
                .string
                .to_string();
            erp_addon_stock::StockError::EngineOnlyDefault { what, field: label }.to_string()
        };
        for (role, id) in [(2, users[2]), (3, users[3])] {
            let login = format!("stock-security-{role}");
            let logged = auth
                .authenticate(LoginRequest::new(
                    &login,
                    format!("{login}-fixture-password"),
                    ClientAddr::InProcess("stock-security-test"),
                ))
                .await
                .unwrap();
            let mut begun = auth
                .begin_session(logged.session().secret(), CredentialOptions::new())
                .await
                .unwrap();
            let principal = begun.tx.env().principal().clone();
            begun.tx.rollback().await.unwrap();
            drop(begun);
            for (model, what, field, value) in defaults.iter().cloned() {
                let line_of = (model == "stock.move.line").then_some(mv);
                let body = serde_json::json!({
                    "vals_list": [json_vals(&vals, line_of)],
                    "context": {format!("default_{field}"): value},
                });
                let response = service
                    .call(erp_rpc::RpcCall::json2(
                        &format!("/json/2/{model}/create"),
                        serde_json::to_vec(&body).unwrap(),
                        erp_rpc::Caller::Principal(principal.clone()),
                    ))
                    .await;
                let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
                assert_ne!(
                    response.status(),
                    200,
                    "RPC {model}.{field}, role {role}: {body}"
                );
                assert_eq!(
                    body["name"], "erp.exceptions.UserError",
                    "RPC {model}.{field}: {body}"
                );
                assert_eq!(
                    body["message"],
                    expected(model, what, field),
                    "RPC {model}.{field}"
                );
            }
            let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
            for (model, what, field, value) in defaults.iter().cloned() {
                let mut create_vals = vals.clone();
                create_vals = match model {
                    "stock.move.line" => create_vals.set_cell("move_id", Cell::Id(mv)),
                    _ => create_vals.set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
                };
                let error = tx
                    .env()
                    .records(model)
                    .unwrap()
                    .with_context([(
                        format!("default_{field}").as_str(),
                        erp_orm::ir_default::json_to_value(&value),
                    )])
                    .create(&mut tx, vec![create_vals])
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), ErrorKind::UserError, "typed {model}.{field}");
                assert_eq!(
                    error.message(),
                    expected(model, what, field),
                    "typed {model}.{field}"
                );
            }
            // a move created through its transfer's command list carries the writer's context
            let error = tx
                .env()
                .records("stock.picking")
                .unwrap()
                .browse([picking])
                .unwrap()
                .with_context([("default_done_at", erp_fields::Value::str(stamp))])
                .write(
                    &mut tx,
                    RawVals::new().set_commands(
                        "move_ids",
                        [Command::Create(
                            vals.clone()
                                .set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
                        )],
                    ),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UserError, "nested write");
            assert_eq!(
                error.message(),
                expected("stock.move", "stock move", "done_at")
            );
            // a transfer created with the key gives its moves none: the move starts empty
            let created = tx
                .env()
                .records("stock.picking")
                .unwrap()
                .with_context([("default_done_at", erp_fields::Value::str(stamp))])
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell(
                                "name",
                                Cell::Str(format!("Defaulted transfer {role}").into()),
                            )
                            .set_cell("company_id", Cell::Id(company))
                            .set_cell("picking_type_id", Cell::Id(picking_type))
                            .set_cell("location_id", Cell::Id(location))
                            .set_cell("location_dest_id", Cell::Id(location))
                            .set_commands(
                                "move_ids",
                                [Command::Create(
                                    vals.clone()
                                        .set_cell("product_qty", Cell::Decimal(Decimal::ZERO)),
                                )],
                            ),
                    ],
                )
                .await
                .unwrap();
            let moves = created.related(&mut tx, "move_ids").await.unwrap();
            assert_eq!(moves.cell(&mut tx, "done_at").await.unwrap(), Cell::Null);
            // the stored default never reaches an engine-owned column; ordinary creates still work
            let line = tx
                .env()
                .records("stock.move.line")
                .unwrap()
                .create(
                    &mut tx,
                    vec![vals.clone().set_cell("move_id", Cell::Id(mv))],
                )
                .await
                .unwrap();
            assert_eq!(
                line.cell(&mut tx, "membership").await.unwrap(),
                Cell::Str("open".into()),
                "role {role}: ir.default reached membership"
            );
            assert_eq!(
                line.cell(&mut tx, "correction_seq").await.unwrap(),
                Cell::Int(0)
            );
            assert_eq!(line.cell(&mut tx, "done_at").await.unwrap(), Cell::Null);
            // a copied move starts with the engine's initial values (the counter is not copied);
            // the product-unit quantity is not copied either and is given here
            let copied = tx
                .env()
                .records("stock.move")
                .unwrap()
                .browse([mv])
                .unwrap()
                .copy(
                    &mut tx,
                    Some(erp_orm::ValueMap::from([(
                        "quantity_product_uom".to_owned(),
                        erp_fields::Value::Decimal(Decimal::ZERO),
                    )])),
                )
                .await
                .unwrap();
            assert_eq!(
                copied.cell(&mut tx, "open_line_version").await.unwrap(),
                Cell::Int(0)
            );
            tx.rollback().await.unwrap();
        }
        db.cleanup().await.unwrap();
    });
}

/// warehouse ownership or a company's derived locations)
#[test]
fn stock_security_derived_assignments_refuse_context_defaults() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let users = roles(&installed).await;
        let company = xid(&installed, "base", "main_company").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let root = create(
            &mut setup,
            "stock.location",
            RawVals::new()
                .set_cell("name", Cell::Str("Default sweep root".into()))
                .set_cell("usage", Cell::Str("view".into())),
        )
        .await;
        let warehouse = create(
            &mut setup,
            "stock.warehouse",
            RawVals::new()
                .set_cell("name", Cell::Str("Default sweep warehouse".into()))
                .set_cell("code", Cell::Str("DSW".into()))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("view_location_id", Cell::Id(root))
                .set_cell("lot_stock_id", Cell::Id(root)),
        )
        .await;
        let currency = setup
            .env()
            .records("res.company")
            .unwrap()
            .browse([company])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        for (field, value) in [
            ("scrap_location_id", erp_fields::Value::Int(root)),
            ("default_stock_location_id", erp_fields::Value::Int(root)),
        ] {
            let error = setup
                .env()
                .with_company(company)
                .unwrap()
                .records("res.company")
                .unwrap()
                .with_context([(format!("default_{field}"), value)])
                .create(
                    &mut setup,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(format!("Default sweep {field}").into()))
                            .set_cell("currency_id", currency.clone()),
                    ],
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::UserError,
                "company {field}: {error}"
            );
            assert_eq!(
                error.message(),
                erp_addon_stock::StockError::CompanyLocationsDerived.to_string(),
                "company {field}"
            );
        }
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure(&db, &installed).await;
        let mut tx = orm.begin(UserId::new(users[3]).unwrap()).await.unwrap();
        let locations = tx.env().records("stock.location").unwrap();
        for (field, value) in [
            ("warehouse_id", erp_fields::Value::Int(warehouse)),
            (
                "warehouse_view_ids",
                erp_fields::Value::list([erp_fields::Value::Int(warehouse)]),
            ),
        ] {
            let error = locations
                .with_context([(format!("default_{field}"), value)])
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(format!("Default sweep {field}").into()))
                            .set_cell("location_id", Cell::Id(root)),
                    ],
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::UserError,
                "location {field}: {error}"
            );
            assert_eq!(
                error.message(),
                erp_addon_stock::StockError::LocationOwnershipDerived.to_string(),
                "location {field}"
            );
        }
        // A location's policy row comes from an insertion trigger, so the context default never
        // reaches it: the location is created with the guard off.
        let location = locations
            .with_context([("default_forbid_negative", erp_fields::Value::Bool(true))])
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Default sweep policy".into()))
                        .set_cell("location_id", Cell::Id(root)),
                ],
            )
            .await
            .unwrap();
        let rows = tx
            .query(
                "SELECT forbid_negative FROM stock_location_policy WHERE location_id = $1",
                &[&location.ids()[0]],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].get::<_, bool>(0),
            "the context default reached the row"
        );
        // A direct policy create with the same context default: the location already has its
        // row, so the unique key refuses it before the negative-stock guard is reached.
        let error = tx
            .savepoint(async |tx| {
                tx.env()
                    .records("stock.location.policy")
                    .unwrap()
                    .with_context([("default_forbid_negative", erp_fields::Value::Bool(true))])
                    .create(
                        tx,
                        vec![RawVals::new().set_cell("location_id", Cell::Id(root))],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(
            (error.kind(), error.message()),
            (
                ErrorKind::Validation,
                "The database refused the change: A location has one stock policy."
            ),
            "policy forbid_negative: {error:?}"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

struct Host;
impl erp_rpc::RpcHost for Host {
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

#[test]
fn stock_security_authenticated_rpc_refuses_employee_lines_and_operator_configuration() {
    run(async {
        use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        roles(&installed).await;
        let orm = secure(&db, &installed).await;
        let auth = Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap();
        let service = erp_rpc::RpcService::new(orm.clone(), Host, erp_rpc::RpcConfig::default());
        for (role, model, method, op, body) in [
            (
                1,
                "stock.move.line",
                "search_count",
                Operation::Read,
                serde_json::json!({"domain": []}),
            ),
            (
                2,
                "product.pricelist",
                "create",
                Operation::Create,
                serde_json::json!({"vals_list": [{"name": "Forbidden RPC price list"}]}),
            ),
        ] {
            let login = format!("stock-security-{role}");
            let logged = auth
                .authenticate(LoginRequest::new(
                    &login,
                    format!("{login}-fixture-password"),
                    ClientAddr::InProcess("stock-security-test"),
                ))
                .await
                .unwrap();
            let mut begun = auth
                .begin_session(logged.session().secret(), CredentialOptions::new())
                .await
                .unwrap();
            let principal = begun.tx.env().principal().clone();
            begun.tx.rollback().await.unwrap();
            drop(begun);
            let response = service
                .call(erp_rpc::RpcCall::json2(
                    &format!("/json/2/{model}/{method}"),
                    serde_json::to_vec(&body).unwrap(),
                    erp_rpc::Caller::Principal(principal),
                ))
                .await;
            let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            assert_eq!(response.status(), 403, "{body}");
            assert_eq!(body["name"], "erp.exceptions.AccessError");
            assert_eq!(body["message"], model_denial_message(&orm, model, op).await);
        }
        db.cleanup().await.unwrap();
    });
}
