// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed product permissions exercised through the ordinary group and principal machinery.

use super::*;

async fn secure_orm(db: &TestDb, installed: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
        .await
        .unwrap();
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
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

async fn user(tx: &mut erp_orm::Transaction, login: &str, company: i64, groups: &[i64]) -> i64 {
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
                        [Command::set(
                            groups.iter().map(|id| RecId::from_real(*id).unwrap()),
                        )],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

fn product_privilege_and_light_groups_are_installed() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;
        let privilege = strings(&orm, "SELECT p.name || '/' || p.placeholder || '/' || g.name
            FROM ir_model_data d JOIN res_groups_privilege p ON p.id=d.res_id
            JOIN res_groups g ON g.privilege_id=p.id
            JOIN ir_model_data gd ON gd.res_id=g.id AND gd.model='res.groups'
            WHERE d.module='product' AND d.name='res_groups_privilege_product'
              AND d.model='res.groups.privilege' AND gd.module='product' AND gd.name='group_product_manager'").await;
        assert_eq!(privilege, ["Products/View/Product Administrator"]);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let groups = tx.env().records("res.groups").unwrap();
        let light: Vec<String> = groups
            .call(&mut tx, "_get_light_group_xmlids", ())
            .await
            .unwrap();
        for xmlid in [
            "base.group_user",
            "uom.group_uom",
            "product.group_product_pricelist",
            "product.group_product_variant",
        ] {
            assert_eq!(
                light.iter().filter(|id| id.as_str() == xmlid).count(),
                1,
                "{xmlid}: {light:?}"
            );
        }
        assert!(!light.iter().any(|id| id == "product.group_product_manager"));
        for xmlid in ["group_product_pricelist", "group_product_variant"] {
            let id = xid(&orm, "product", xmlid).await;
            let is_light: bool = groups
                .browse([id])
                .unwrap()
                .call(&mut tx, "_is_light_groups", ())
                .await
                .unwrap();
            assert!(is_light, "{xmlid}");
        }
        tx.rollback().await.unwrap();
    });
}

#[test]
fn product_light_groups_compose_without_privilege_data() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let light: Vec<String> = tx
            .env()
            .records("res.groups")
            .unwrap()
            .call(&mut tx, "_get_light_group_xmlids", ())
            .await
            .unwrap();
        for xmlid in [
            "base.group_user",
            "uom.group_uom",
            "product.group_product_pricelist",
            "product.group_product_variant",
        ] {
            assert_eq!(
                light.iter().filter(|id| id.as_str() == xmlid).count(),
                1,
                "{xmlid}: {light:?}"
            );
        }
        tx.rollback().await.unwrap();
    });
}

fn legacy_product_upgrade_links_privilege_once_and_preserves_customization() {
    big_stack(async {
        for custom in [false, true] {
            let db = TestDb::create().await.unwrap();
            let (system, orm) = install(&db).await;
            let manager = xid(&orm, "product", "group_product_manager").await;
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let custom_privilege = if custom {
                Some(
                    tx.env()
                        .records("res.groups.privilege")
                        .unwrap()
                        .create(
                            &mut tx,
                            vec![
                                RawVals::new()
                                    .set_cell("name", Cell::Str("Local catalog rights".into())),
                            ],
                        )
                        .await
                        .unwrap()
                        .ids()[0],
                )
            } else {
                None
            };
            tx.env()
                .records("res.groups")
                .unwrap()
                .browse([manager])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new().set_cell(
                        "privilege_id",
                        custom_privilege.map_or(Cell::Null, Cell::Id),
                    ),
                )
                .await
                .unwrap();
            if !custom {
                // Simulate a legacy installation with neither the new row nor its XML identity.
                let privilege = xid(&orm, "product", "res_groups_privilege_product").await;
                tx.env()
                    .records("res.groups.privilege")
                    .unwrap()
                    .browse([privilege])
                    .unwrap()
                    .unlink(&mut tx)
                    .await
                    .unwrap();
                let remaining = tx.query("SELECT id FROM ir_model_data WHERE module='product' AND name='res_groups_privilege_product'", &[]).await.unwrap();
                assert!(
                    remaining.is_empty(),
                    "unlink hook removes the new identity too"
                );
            }
            tx.execute(
                "UPDATE ir_module_module SET latest_version='1.1.0' WHERE name='product'",
                &[],
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            drop(tx);
            let upgraded = system
                .button_immediate_upgrade(
                    &system.system_env(Context::new()),
                    &[ModuleName::new("product").unwrap()],
                )
                .await
                .unwrap();
            assert!(
                upgraded.post_commit.is_clean(),
                "{:?}",
                upgraded.post_commit
            );
            let privilege = xid(&upgraded.orm, "product", "res_groups_privilege_product").await;
            let expected = custom_privilege.unwrap_or(privilege);
            let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
            let group = tx
                .env()
                .records("res.groups")
                .unwrap()
                .browse([manager])
                .unwrap();
            assert_eq!(
                group.cell(&mut tx, "privilege_id").await.unwrap(),
                Cell::Id(expected)
            );
            let p = tx
                .env()
                .records("res.groups.privilege")
                .unwrap()
                .browse([privilege])
                .unwrap();
            p.write(
                &mut tx,
                RawVals::new().set_cell("name", Cell::Str("Catalog rights edited locally".into())),
            )
            .await
            .unwrap();
            // Once the migration has run, deliberately removing its link is also an administrator choice.
            group
                .write(&mut tx, RawVals::new().set_cell("privilege_id", Cell::Null))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            drop(tx);
            let again = system
                .button_immediate_upgrade(
                    &system.system_env(Context::new()),
                    &[ModuleName::new("product").unwrap()],
                )
                .await
                .unwrap();
            assert!(again.post_commit.is_clean(), "{:?}", again.post_commit);
            let mut tx = again.orm.begin(SUPERUSER_ID).await.unwrap();
            assert_eq!(
                tx.env()
                    .records("res.groups")
                    .unwrap()
                    .browse([manager])
                    .unwrap()
                    .cell(&mut tx, "privilege_id")
                    .await
                    .unwrap(),
                Cell::Null
            );
            assert_eq!(
                tx.env()
                    .records("res.groups.privilege")
                    .unwrap()
                    .browse([privilege])
                    .unwrap()
                    .cell(&mut tx, "name")
                    .await
                    .unwrap(),
                Cell::Str("Catalog rights edited locally".into())
            );
            tx.rollback().await.unwrap();
        }
    });
}

use erp_sdk::{Domain, ErrorKind, Operation, Operator};

struct Case {
    model: &'static str,
    create: RawVals,
    change: RawVals,
    field: &'static str,
    before: Cell,
    after: Cell,
    id: i64,
}

async fn create(tx: &mut erp_orm::Transaction, model: &str, vals: RawVals) -> i64 {
    let main: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.env()
        .with_company(main)
        .unwrap()
        .with_context([("create_product_product", erp_sdk::Value::Bool(false))])
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn cases(tx: &mut erp_orm::Transaction, company: i64) -> Vec<Case> {
    let name = |s: &str| RawVals::new().set_cell("name", Cell::Str(s.into()));
    let template = create(
        tx,
        "product.template",
        name("Access support template").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let variant = create(
        tx,
        "product.product",
        RawVals::new().set_cell("product_tmpl_id", Cell::Id(template)),
    )
    .await;
    let variant_parent = create(
        tx,
        "product.template",
        name("Variant ACL support").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let unit = create(tx, "uom.uom", name("Access support unit")).await;
    let vendor = create(tx, "res.partner", name("Access support vendor")).await;
    let pricelist = create(
        tx,
        "product.pricelist",
        name("Access support pricelist").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let attribute = create(
        tx,
        "product.attribute",
        name("Access support attribute").set_cell("create_variant", Cell::Str("no_variant".into())),
    )
    .await;
    let value = create(
        tx,
        "product.attribute.value",
        name("Access support value").set_cell("attribute_id", Cell::Id(attribute)),
    )
    .await;
    let line_template = create(
        tx,
        "product.template",
        name("Line ACL support").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let value_template = create(
        tx,
        "product.template",
        name("Value ACL support").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let line = create(
        tx,
        "product.template.attribute.line",
        RawVals::new()
            .set_cell("product_tmpl_id", Cell::Id(value_template))
            .set_cell("attribute_id", Cell::Id(attribute))
            .set_cell("active", Cell::Bool(false)),
    )
    .await;
    let combo = create(
        tx,
        "product.combo",
        name("Access support combo").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let currency = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(tx, "currency_id")
        .await
        .unwrap();
    let mut specs = vec![
        (
            "product.category",
            name("ACL category").set_cell("company_id", Cell::Id(company)),
            "name",
            Cell::Str("ACL category".into()),
            Cell::Str("Changed category".into()),
        ),
        (
            "product.template",
            name("ACL template").set_cell("company_id", Cell::Id(company)),
            "name",
            Cell::Str("ACL template".into()),
            Cell::Str("Changed template".into()),
        ),
        (
            "product.product",
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(variant_parent))
                .set_cell("active", Cell::Bool(false))
                .set_cell("default_code", Cell::Str("ACL-SKU".into())),
            "default_code",
            Cell::Str("ACL-SKU".into()),
            Cell::Str("CHANGED-SKU".into()),
        ),
        (
            "product.supplierinfo",
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("product_tmpl_id", Cell::Id(template))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("price", Cell::Decimal(erp_sdk::Decimal::ONE))
                .set_cell("currency_id", currency.clone())
                .set_cell("uom_id", Cell::Id(unit)),
            "price",
            Cell::Decimal(erp_sdk::Decimal::ONE),
            Cell::Decimal(erp_sdk::Decimal::new(2, 0)),
        ),
        (
            "product.pricelist",
            name("ACL pricelist")
                .set_cell("company_id", Cell::Id(company))
                .set_cell("currency_id", currency.clone()),
            "name",
            Cell::Str("ACL pricelist".into()),
            Cell::Str("Changed pricelist".into()),
        ),
        (
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("fixed_price", Cell::Decimal(erp_sdk::Decimal::ONE)),
            "fixed_price",
            Cell::Decimal(erp_sdk::Decimal::ONE),
            Cell::Decimal(erp_sdk::Decimal::new(2, 0)),
        ),
        (
            "product.attribute",
            name("ACL attribute"),
            "name",
            Cell::Str("ACL attribute".into()),
            Cell::Str("Changed attribute".into()),
        ),
        (
            "product.attribute.value",
            name("ACL attribute value").set_cell("attribute_id", Cell::Id(attribute)),
            "name",
            Cell::Str("ACL attribute value".into()),
            Cell::Str("Changed attribute value".into()),
        ),
        (
            "product.template.attribute.line",
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(line_template))
                .set_cell("attribute_id", Cell::Id(attribute))
                .set_commands(
                    "value_ids",
                    [Command::set([RecId::from_real(value).unwrap()])],
                ),
            "active",
            Cell::Bool(true),
            Cell::Bool(false),
        ),
        (
            "product.template.attribute.value",
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(value_template))
                .set_cell("attribute_line_id", Cell::Id(line))
                .set_cell("product_attribute_value_id", Cell::Id(value))
                .set_cell("attribute_id", Cell::Id(attribute))
                .set_cell("price_extra", Cell::Decimal(erp_sdk::Decimal::ZERO)),
            "price_extra",
            Cell::Decimal(erp_sdk::Decimal::ZERO),
            Cell::Decimal(erp_sdk::Decimal::ONE),
        ),
        (
            "product.combo",
            name("ACL combo").set_cell("company_id", Cell::Id(company)),
            "name",
            Cell::Str("ACL combo".into()),
            Cell::Str("Changed combo".into()),
        ),
        (
            "product.combo.item",
            RawVals::new()
                .set_cell("combo_id", Cell::Id(combo))
                .set_cell("product_id", Cell::Id(variant))
                .set_cell("extra_price", Cell::Decimal(erp_sdk::Decimal::ZERO)),
            "extra_price",
            Cell::Decimal(erp_sdk::Decimal::ZERO),
            Cell::Decimal(erp_sdk::Decimal::ONE),
        ),
        (
            "product.uom",
            RawVals::new()
                .set_cell("product_id", Cell::Id(variant))
                .set_cell("uom_id", Cell::Id(unit))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("barcode", Cell::Str("ACL-PACK-ORIGINAL".into())),
            "barcode",
            Cell::Str("ACL-PACK-ORIGINAL".into()),
            Cell::Str("ACL-PACK-CHANGED".into()),
        ),
        (
            "uom.uom",
            name("ACL unit"),
            "name",
            Cell::Str("ACL unit".into()),
            Cell::Str("Changed unit".into()),
        ),
    ];
    let mut result = Vec::new();
    for (model, vals, field, before, after) in specs.drain(..) {
        let id = create(tx, model, vals.clone()).await;
        result.push(Case {
            model,
            create: vals,
            change: RawVals::new().set_cell(field, after.clone()),
            field,
            before,
            after,
            id,
        });
    }
    result
}

async fn allowing_group_names(orm: &Orm, model: &str, operation: Operation) -> Vec<String> {
    // Independent database metadata supplies the expected group order/text, not the policy's error renderer.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let op = match operation {
        Operation::Read => "r",
        Operation::Write => "u",
        Operation::Create => "c",
        Operation::Unlink => "d",
    };
    let result = tx
        .query(
            "WITH RECURSIVE allowing(id) AS (
            SELECT a.group_id FROM ir_access a JOIN ir_model m ON m.id=a.model_id
            WHERE m.model=$1 AND a.active AND a.domain IS NULL AND position($2 in a.operation)>0
            UNION SELECT r.gid FROM res_groups_implied_rel r JOIN allowing a ON a.id=r.hid
        ) SELECT DISTINCT p.sequence, p.name, g.sequence, g.name, g.id
        FROM allowing a JOIN res_groups g ON g.id=a.id
        LEFT JOIN res_groups_privilege p ON p.id=g.privilege_id
        ORDER BY p.sequence, p.name, g.sequence, g.name, g.id",
            &[&model, &op],
        )
        .await
        .unwrap();
    let names = result
        .iter()
        .map(|row| {
            let privilege: Option<String> = row.get(1);
            let group: String = row.get(3);
            match privilege {
                Some(p) => format!("{p} / {group}"),
                None => group,
            }
        })
        .collect();
    tx.rollback().await.unwrap();
    names
}

fn model_denial(orm: &Orm, model: &str, op: Operation, groups: &[String], error: &erp_sdk::Error) {
    assert_eq!(error.kind(), ErrorKind::Access, "{model}/{op:?}: {error}");
    let verb = match op {
        Operation::Read => "access",
        Operation::Write => "modify",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    };
    let description = &orm.registry().model(model).unwrap().description;
    let list = groups
        .iter()
        .map(|g| format!("\t- {g}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !list.is_empty(),
        "the denied operation has real allowing groups"
    );
    assert_eq!(
        error.message(),
        format!(
            "Your access rights do not let you {verb} '{description}' ({model}) records.\n\nGroups that may do this:\n{list}\n\nAsk an administrator if you need this access."
        )
    );
}

fn product_model_permissions_are_enforced_for_real_users() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let main = xid(&installed, "base", "main_company").await;
        let system = xid(&installed, "base", "group_system").await;
        let manager = xid(&installed, "product", "group_product_manager").await;
        let internal = xid(&installed, "base", "group_user").await;
        let portal = xid(&installed, "base", "group_portal").await;
        let feature = xid(&installed, "product", "group_product_variant").await;
        let pricelist_feature = xid(&installed, "product", "group_product_pricelist").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        user(&mut setup, "access-bootstrap", main, &[system]).await;
        let reader = user(
            &mut setup,
            "access-reader",
            main,
            &[internal, feature, pricelist_feature],
        )
        .await;
        let writer = user(&mut setup, "access-writer", main, &[manager]).await;
        let outsider = user(&mut setup, "access-outsider", main, &[portal]).await;
        let admin = user(&mut setup, "access-admin", main, &[system]).await;
        for id in [reader, writer, outsider, admin] {
            assert_ne!(UserId::new(id).unwrap(), SUPERUSER_ID);
        }
        let cases = cases(&mut setup, main).await;
        assert_eq!(cases.iter().map(|c| c.model).collect::<Vec<_>>(), MODELS);
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure_orm(&db, &installed).await;
        for case in &cases {
            let mut tx = orm.begin(UserId::new(reader).unwrap()).await.unwrap();
            let record = tx
                .env()
                .records(case.model)
                .unwrap()
                .browse([case.id])
                .unwrap();
            assert_eq!(
                record.cell(&mut tx, case.field).await.unwrap(),
                case.before,
                "{} reader",
                case.model
            );
            tx.rollback().await.unwrap();
            drop(tx);
            for (id, ops) in [
                (
                    reader,
                    &[Operation::Create, Operation::Write, Operation::Unlink][..],
                ),
                (
                    outsider,
                    &[
                        Operation::Read,
                        Operation::Create,
                        Operation::Write,
                        Operation::Unlink,
                    ][..],
                ),
            ] {
                for op in ops {
                    let groups = allowing_group_names(&orm, case.model, *op).await;
                    let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
                    let model = tx
                        .env()
                        .with_context([("create_product_product", erp_sdk::Value::Bool(false))])
                        .records(case.model)
                        .unwrap();
                    let record = model.browse([case.id]).unwrap();
                    let error = match op {
                        Operation::Read => record.cell(&mut tx, case.field).await.unwrap_err(),
                        Operation::Create => model
                            .create(&mut tx, vec![case.create.clone()])
                            .await
                            .unwrap_err(),
                        Operation::Write => record
                            .write(&mut tx, case.change.clone())
                            .await
                            .unwrap_err(),
                        Operation::Unlink => record.unlink(&mut tx).await.unwrap_err(),
                    };
                    model_denial(&orm, case.model, *op, &groups, &error);
                    tx.rollback().await.unwrap();
                }
            }
            for id in [writer, admin] {
                let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
                assert!(
                    tx.has_group(&tx.env(), "product.group_product_manager")
                        .await
                        .unwrap()
                );
                let model = tx
                    .env()
                    .with_context([("create_product_product", erp_sdk::Value::Bool(false))])
                    .records(case.model)
                    .unwrap();
                let record = model.browse([case.id]).unwrap();
                assert_eq!(record.cell(&mut tx, case.field).await.unwrap(), case.before);
                record.write(&mut tx, case.change.clone()).await.unwrap();
                assert_eq!(record.cell(&mut tx, case.field).await.unwrap(), case.after);
                record.unlink(&mut tx).await.unwrap();
                let created = model
                    .create(&mut tx, vec![case.create.clone()])
                    .await
                    .unwrap();
                assert_eq!(created.ids().len(), 1, "{} manager create", case.model);
                assert_eq!(
                    created.cell(&mut tx, case.field).await.unwrap(),
                    case.before
                );
                tx.rollback().await.unwrap();
            }
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            assert_eq!(
                tx.env()
                    .records(case.model)
                    .unwrap()
                    .browse([case.id])
                    .unwrap()
                    .cell(&mut tx, case.field)
                    .await
                    .unwrap(),
                case.before,
                "{} refused changes rolled back",
                case.model
            );
            tx.rollback().await.unwrap();
        }
    });
}

struct ScopedRows {
    model: &'static str,
    field: &'static str,
    rows: Vec<(i64, Cell)>,
    change: RawVals,
    owner_field: &'static str,
    owners: Vec<Cell>,
}

fn record_denial(
    orm: &Orm,
    model: &str,
    user: i64,
    login: &str,
    op: Operation,
    error: &erp_sdk::Error,
) {
    assert_eq!(error.kind(), ErrorKind::Access, "{model}/{op:?}: {error}");
    let desc = &orm.registry().model(model).unwrap().description;
    let verb = match op {
        Operation::Read => "read",
        Operation::Write => "write",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    };
    let hint = if [
        "product.template",
        "product.category",
        "product.pricelist",
        "product.supplierinfo",
        "product.uom",
        "product.combo",
    ]
    .contains(&model)
    {
        "\n\nThe record belongs to a company outside your allowed companies."
    } else {
        ""
    };
    assert_eq!(
        error.message(),
        format!(
            "Access rules protect some of these records.\n\n{login} (id={user}) has no '{verb}' access to:\n- {desc} ({model})\n\nAsk an administrator if you need access to these records.{hint}"
        )
    );
}

/// A create under a hidden owner is refused either for the created model or, when a default of
/// the new row reads the hidden company first (a pricelist's currency), for that company.
fn create_denial(orm: &Orm, model: &str, user: i64, login: &str, error: &erp_sdk::Error) {
    if error.message().contains("- Company (res.company)") {
        assert_eq!(error.kind(), ErrorKind::Access, "{model}/create: {error}");
        assert_eq!(
            error.message(),
            format!(
                "Access rules protect some of these records.\n\n{login} (id={user}) has no 'read' access to:\n- Company (res.company)\n\nAsk an administrator if you need access to these records."
            ),
            "{model}/create"
        );
    } else {
        record_denial(orm, model, user, login, Operation::Create, error);
    }
}

fn company_rules_cover_every_catalog_owner_as_real_users() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let a = xid(&installed, "base", "main_company").await;
        let system = xid(&installed, "base", "group_system").await;
        let manager = xid(&installed, "product", "group_product_manager").await;
        let internal = xid(&installed, "base", "group_user").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        user(&mut setup, "scope-bootstrap", a, &[system]).await;
        let company_model = setup.env().records("res.company").unwrap();
        let currency = company_model
            .browse([a])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let b = company_model
            .with_context([("default_parent_id", erp_sdk::Value::Int(a))])
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Access branch B".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let x = company_model
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Access unrelated X".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut users = Vec::new();
        for (company, label) in [(a, "A"), (b, "B"), (x, "X")] {
            for (group, role) in [(internal, "reader"), (manager, "manager")] {
                let login = format!("scope-{label}-{role}");
                let id = user(&mut setup, &login, company, &[group]).await;
                assert_ne!(UserId::new(id).unwrap(), SUPERUSER_ID);
                users.push((company, login, id));
            }
        }
        let name = |s: &str| RawVals::new().set_cell("name", Cell::Str(s.into()));
        let attribute = create(
            &mut setup,
            "product.attribute",
            name("Scope attribute").set_cell("create_variant", Cell::Str("no_variant".into())),
        )
        .await;
        let value = create(
            &mut setup,
            "product.attribute.value",
            name("Scope value").set_cell("attribute_id", Cell::Id(attribute)),
        )
        .await;
        // Fresh line and value targets for the hidden-owner creates, clear of existing pairs.
        let spare_attribute = create(
            &mut setup,
            "product.attribute",
            name("Scope spare attribute")
                .set_cell("create_variant", Cell::Str("no_variant".into())),
        )
        .await;
        let spare_value = create(
            &mut setup,
            "product.attribute.value",
            name("Scope spare value").set_cell("attribute_id", Cell::Id(attribute)),
        )
        .await;
        let vendor = create(&mut setup, "res.partner", name("Scope vendor")).await;
        let unit = create(&mut setup, "uom.uom", name("Scope unit")).await;
        let mut groups: Vec<ScopedRows> = [
            ("product.template", "name", "company_id", "sequence"),
            (
                "product.product",
                "default_code",
                "product_tmpl_id",
                "default_code",
            ),
            ("product.category", "name", "company_id", "name"),
            ("product.pricelist", "name", "company_id", "sequence"),
            (
                "product.pricelist.item",
                "pricelist_id",
                "pricelist_id",
                "fixed_price",
            ),
            (
                "product.supplierinfo",
                "product_code",
                "company_id",
                "sequence",
            ),
            ("product.uom", "barcode", "company_id", "barcode"),
            ("product.combo", "name", "company_id", "sequence"),
            ("product.combo.item", "combo_id", "combo_id", "extra_price"),
            (
                "product.template.attribute.line",
                "product_tmpl_id",
                "product_tmpl_id",
                "active",
            ),
            (
                "product.template.attribute.value",
                "product_tmpl_id",
                "product_tmpl_id",
                "price_extra",
            ),
        ]
        .into_iter()
        .map(|(model, field, owner_field, change_field)| ScopedRows {
            model,
            field,
            owner_field,
            rows: Vec::new(),
            owners: Vec::new(),
            change: RawVals::new().set_cell(
                change_field,
                match change_field {
                    "name" | "default_code" | "barcode" => Cell::Str("scope-mutated".into()),
                    "active" => Cell::Bool(false),
                    "fixed_price" | "extra_price" | "price_extra" => {
                        Cell::Decimal(erp_sdk::Decimal::ONE)
                    }
                    "sequence" => Cell::Int(8),
                    other => panic!("unexpected test field {other}"),
                },
            ),
        })
        .collect();
        for (company, label) in [
            (Some(a), "A"),
            (Some(b), "B"),
            (Some(x), "X"),
            (None, "Shared"),
        ] {
            let company = company.map_or(Cell::Null, Cell::Id);
            let template_name = format!("Scoped template {label}");
            let template = create(
                &mut setup,
                "product.template",
                name(&template_name).set_cell("company_id", company.clone()),
            )
            .await;
            let code = format!("SCOPE-{label}");
            let variant = create(
                &mut setup,
                "product.product",
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("default_code", Cell::Str(code.clone().into())),
            )
            .await;
            let category_name = format!("Scoped category {label}");
            let category = create(
                &mut setup,
                "product.category",
                name(&category_name).set_cell("company_id", company.clone()),
            )
            .await;
            let pricelist_name = format!("Scoped pricelist {label}");
            let pricelist = create(
                &mut setup,
                "product.pricelist",
                name(&pricelist_name).set_cell("company_id", company.clone()),
            )
            .await;
            let item = create(
                &mut setup,
                "product.pricelist.item",
                RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
            )
            .await;
            let supplier = create(
                &mut setup,
                "product.supplierinfo",
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("company_id", company.clone())
                    .set_cell("product_code", Cell::Str(code.clone().into())),
            )
            .await;
            let barcode = format!("SCOPE-PACK-{label}");
            let packaging = create(
                &mut setup,
                "product.uom",
                RawVals::new()
                    .set_cell("product_id", Cell::Id(variant))
                    .set_cell("uom_id", Cell::Id(unit))
                    .set_cell("company_id", company.clone())
                    .set_cell("barcode", Cell::Str(barcode.clone().into())),
            )
            .await;
            let combo_name = format!("Scoped combo {label}");
            let combo = create(
                &mut setup,
                "product.combo",
                name(&combo_name).set_cell("company_id", company.clone()),
            )
            .await;
            let combo_item = create(
                &mut setup,
                "product.combo.item",
                RawVals::new()
                    .set_cell("combo_id", Cell::Id(combo))
                    .set_cell("product_id", Cell::Id(variant)),
            )
            .await;
            let line = create(
                &mut setup,
                "product.template.attribute.line",
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("attribute_id", Cell::Id(attribute))
                    .set_cell("active", Cell::Bool(false)),
            )
            .await;
            let ptav = create(
                &mut setup,
                "product.template.attribute.value",
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("attribute_line_id", Cell::Id(line))
                    .set_cell("attribute_id", Cell::Id(attribute))
                    .set_cell("product_attribute_value_id", Cell::Id(value)),
            )
            .await;
            let entries = [
                (template, Cell::Str(template_name.into()), company.clone()),
                (variant, Cell::Str(code.clone().into()), Cell::Id(template)),
                (category, Cell::Str(category_name.into()), company.clone()),
                (pricelist, Cell::Str(pricelist_name.into()), company.clone()),
                (item, Cell::Id(pricelist), Cell::Id(pricelist)),
                (supplier, Cell::Str(code.into()), company.clone()),
                (packaging, Cell::Str(barcode.into()), company.clone()),
                (combo, Cell::Str(combo_name.into()), company),
                (combo_item, Cell::Id(combo), Cell::Id(combo)),
                (line, Cell::Id(template), Cell::Id(template)),
                (ptav, Cell::Id(template), Cell::Id(template)),
            ];
            for (group, (id, field, owner)) in groups.iter_mut().zip(entries) {
                group.rows.push((id, field));
                group.owners.push(owner);
            }
        }
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure_orm(&db, &installed).await;
        for (company, login, id) in &users {
            let visible = if *company == b {
                vec![0, 1, 3]
            } else if *company == a {
                vec![0, 3]
            } else {
                vec![2, 3]
            };
            for group in &groups {
                let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
                let model = tx
                    .env()
                    .with_context([("active_test", erp_sdk::Value::Bool(false))])
                    .records(group.model)
                    .unwrap();
                let all_ids: Vec<i64> = group.rows.iter().map(|r| r.0).collect();
                let found = model
                    .search(
                        &mut tx,
                        &Domain::condition("id", Operator::In, all_ids).unwrap(),
                    )
                    .await
                    .unwrap();
                let mut actual = found.ids();
                actual.sort();
                let mut expected: Vec<i64> = visible.iter().map(|i| group.rows[*i].0).collect();
                expected.sort();
                assert_eq!(actual, expected, "{}/{login}", group.model);
                for index in &visible {
                    let (row, value) = &group.rows[*index];
                    assert_eq!(
                        model
                            .browse([*row])
                            .unwrap()
                            .cell(&mut tx, group.field)
                            .await
                            .unwrap(),
                        *value,
                        "{}/{login}",
                        group.model
                    );
                }
                tx.rollback().await.unwrap();
                drop(tx);
                for (index, (row, _)) in group
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !visible.contains(index))
                {
                    let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
                    let error = tx
                        .env()
                        .records(group.model)
                        .unwrap()
                        .browse([*row])
                        .unwrap()
                        .cell(&mut tx, group.field)
                        .await
                        .unwrap_err();
                    record_denial(&orm, group.model, *id, login, Operation::Read, &error);
                    tx.rollback().await.unwrap();
                    if login.ends_with("manager") {
                        for op in [Operation::Write, Operation::Unlink] {
                            let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
                            let record = tx
                                .env()
                                .records(group.model)
                                .unwrap()
                                .browse([*row])
                                .unwrap();
                            let error = match op {
                                Operation::Write => record
                                    .write(&mut tx, group.change.clone())
                                    .await
                                    .unwrap_err(),
                                Operation::Unlink => record.unlink(&mut tx).await.unwrap_err(),
                                _ => panic!("mutation only"),
                            };
                            record_denial(&orm, group.model, *id, login, op, &error);
                            tx.rollback().await.unwrap();
                        }
                        // A create under the hidden owner or company is refused for every model.
                        let owner = group.owners[index].clone();
                        let shared_template = Cell::Id(groups[0].rows[3].0);
                        let shared_variant = Cell::Id(groups[1].rows[3].0);
                        let mut vals = RawVals::new().set_cell(group.owner_field, owner);
                        vals = match group.model {
                            "product.template" | "product.category" | "product.pricelist"
                            | "product.combo" => {
                                vals.set_cell("name", Cell::Str("Forbidden company row".into()))
                            }
                            "product.product" | "product.pricelist.item" => vals,
                            "product.supplierinfo" => vals
                                .set_cell("partner_id", Cell::Id(vendor))
                                .set_cell("product_tmpl_id", shared_template),
                            "product.uom" => vals
                                .set_cell("product_id", shared_variant)
                                .set_cell("uom_id", Cell::Id(unit))
                                .set_cell(
                                    "barcode",
                                    Cell::Str(format!("SCOPE-FORBIDDEN-{login}-{index}").into()),
                                ),
                            "product.combo.item" => vals.set_cell("product_id", shared_variant),
                            "product.template.attribute.line" => vals
                                .set_cell("attribute_id", Cell::Id(spare_attribute))
                                .set_cell("active", Cell::Bool(false)),
                            "product.template.attribute.value" => vals
                                .set_cell("attribute_line_id", Cell::Id(groups[9].rows[index].0))
                                .set_cell("attribute_id", Cell::Id(attribute))
                                .set_cell("product_attribute_value_id", Cell::Id(spare_value)),
                            other => panic!("no hidden-owner create for {other}"),
                        };
                        let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
                        let model = tx
                            .env()
                            .with_context([("active_test", erp_sdk::Value::Bool(false))])
                            .records(group.model)
                            .unwrap();
                        let error = tx
                            .savepoint(async |tx| model.create(tx, vec![vals]).await)
                            .await
                            .unwrap_err();
                        create_denial(&orm, group.model, *id, login, &error);
                        tx.rollback().await.unwrap();
                        // A visible child/direct record cannot be moved into the hidden owner's company.
                        let source = group.rows[3].0;
                        let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
                        let error = tx
                            .env()
                            .records(group.model)
                            .unwrap()
                            .browse([source])
                            .unwrap()
                            .write(
                                &mut tx,
                                RawVals::new()
                                    .set_cell(group.owner_field, group.owners[index].clone()),
                            )
                            .await
                            .unwrap_err();
                        assert_eq!(
                            error.kind(),
                            ErrorKind::Access,
                            "{}/{login} reparent: {error}",
                            group.model
                        );
                        record_denial(&orm, group.model, *id, login, Operation::Write, &error);
                        tx.rollback().await.unwrap();
                    }
                }
            }
        }
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        for group in &groups {
            for (row, value) in &group.rows {
                assert_eq!(
                    tx.env()
                        .records(group.model)
                        .unwrap()
                        .browse([*row])
                        .unwrap()
                        .cell(&mut tx, group.field)
                        .await
                        .unwrap(),
                    *value,
                    "{} unchanged",
                    group.model
                );
            }
        }
        tx.rollback().await.unwrap();
    });
}

fn product_access_migration_refuses_missing_or_mistyped_identity() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;
        for wrong_model in [false, true] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            if wrong_model {
                tx.execute("UPDATE ir_model_data SET model='res.partner' WHERE module='product' AND name='group_product_manager'", &[]).await.unwrap();
            } else {
                tx.execute("DELETE FROM ir_model_data WHERE module='product' AND name='group_product_manager'", &[]).await.unwrap();
            }
            let error = erp_addon_product::migrations::migrate_product_access(
                &mut tx,
                &erp_sdk::migrations::SemverVersion::new(1, 1, 0),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                if wrong_model {
                    "Product access migration requires product.group_product_manager to name res.groups, found res.partner."
                } else {
                    "Product access migration requires exactly one product.group_product_manager identity."
                }
            );
            tx.rollback().await.unwrap();
        }
    });
}

fn settings_administrator_keeps_product_management_across_install_order() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let init = system.initialize(InitOptions::new()).await.unwrap();
        let main = xid(&init.orm, "base", "main_company").await;
        let system_group = xid(&init.orm, "base", "group_system").await;
        let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
        user(&mut tx, "order-bootstrap", main, &[system_group]).await;
        let before = user(&mut tx, "admin-before-product", main, &[system_group]).await;
        tx.commit().await.unwrap();
        drop(tx);
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("product").unwrap()],
            )
            .await
            .unwrap();
        assert!(
            installed.post_commit.is_clean(),
            "{:?}",
            installed.post_commit
        );
        let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
        let after = user(&mut tx, "admin-after-product", main, &[system_group]).await;
        tx.commit().await.unwrap();
        drop(tx);
        let orm = secure_orm(&db, &installed.orm).await;
        for id in [before, after] {
            assert_ne!(UserId::new(id).unwrap(), SUPERUSER_ID);
            let mut tx = orm.begin(UserId::new(id).unwrap()).await.unwrap();
            assert!(
                tx.has_group(&tx.env(), "product.group_product_manager")
                    .await
                    .unwrap()
            );
            let group = tx.env().records("product.category").unwrap();
            let record = group
                .create(
                    &mut tx,
                    vec![
                        RawVals::new().set_cell("name", Cell::Str("Administrator category".into())),
                    ],
                )
                .await
                .unwrap();
            record
                .write(
                    &mut tx,
                    RawVals::new().set_cell("name", Cell::Str("Changed by administrator".into())),
                )
                .await
                .unwrap();
            assert_eq!(
                record.cell(&mut tx, "name").await.unwrap(),
                Cell::Str("Changed by administrator".into())
            );
            record.unlink(&mut tx).await.unwrap();
            tx.rollback().await.unwrap();
        }
    });
}

async fn authenticated(orm: &Orm, login: &str) -> erp_orm::Principal {
    let auth = Auth::new(
        orm.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let logged = auth
        .authenticate(LoginRequest::new(
            login,
            format!("{login}-fixture-password"),
            ClientAddr::InProcess("product-access-test"),
        ))
        .await
        .unwrap();
    let mut begun = auth
        .begin_session(logged.session().secret(), CredentialOptions::new())
        .await
        .unwrap();
    let principal = begun.tx.env().principal().clone();
    assert_ne!(principal.subject(), SUPERUSER_ID);
    begun.tx.rollback().await.unwrap();
    principal
}

async fn rpc(
    service: &RpcService,
    principal: &erp_orm::Principal,
    method: &str,
    args: serde_json::Value,
) -> (u16, serde_json::Value) {
    let response = service
        .call(RpcCall::json2(
            &format!("/json/2/product.template/{method}"),
            serde_json::to_vec(&args).unwrap(),
            Caller::Principal(principal.clone()),
        ))
        .await;
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    (response.status(), body)
}

fn product_permissions_and_company_boundaries_hold_over_json2() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let main = xid(&installed, "base", "main_company").await;
        let system = xid(&installed, "base", "group_system").await;
        let manager = xid(&installed, "product", "group_product_manager").await;
        let internal = xid(&installed, "base", "group_user").await;
        let portal = xid(&installed, "base", "group_portal").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        user(&mut setup, "rpc-access-bootstrap", main, &[system]).await;
        for (login, group) in [
            ("rpc-access-reader", internal),
            ("rpc-access-manager", manager),
            ("rpc-access-outsider", portal),
        ] {
            user(&mut setup, login, main, &[group]).await;
        }
        let company_model = setup.env().records("res.company").unwrap();
        let currency = company_model
            .browse([main])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let other = company_model
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("RPC unrelated company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let own = create(
            &mut setup,
            "product.template",
            RawVals::new()
                .set_cell("name", Cell::Str("RPC own product".into()))
                .set_cell("company_id", Cell::Id(main)),
        )
        .await;
        let hidden = create(
            &mut setup,
            "product.template",
            RawVals::new()
                .set_cell("name", Cell::Str("RPC hidden product".into()))
                .set_cell("company_id", Cell::Id(other)),
        )
        .await;
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure_orm(&db, &installed).await;
        let mut audit = orm
            .begin_opts(
                erp_orm::BeginOptions::new(SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await
            .unwrap();
        audit.install_audit_schema().await.unwrap();
        audit.commit().await.unwrap();
        drop(audit);
        let reader = authenticated(&orm, "rpc-access-reader").await;
        let writer = authenticated(&orm, "rpc-access-manager").await;
        let outsider = authenticated(&orm, "rpc-access-outsider").await;
        let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
        for principal in [&reader, &writer] {
            let (status, found) = rpc(
                &service,
                principal,
                "search",
                serde_json::json!({"domain":[["id","in",[own,hidden]]]}),
            )
            .await;
            assert_eq!(status, 200, "{found}");
            assert_eq!(found, serde_json::json!([own]));
            let (status, found) = rpc(
                &service,
                principal,
                "read",
                serde_json::json!({"ids":[own],"fields":["name"],"load":null}),
            )
            .await;
            assert_eq!(status, 200, "{found}");
            assert_eq!(found[0]["name"], "RPC own product");
            let (status, error) = rpc(
                &service,
                principal,
                "read",
                serde_json::json!({"ids":[hidden],"fields":["name"],"load":null}),
            )
            .await;
            assert_eq!(status, 403, "{error}");
            assert_eq!(error["name"], "erp.exceptions.AccessError");
            let login = if principal.subject() == reader.subject() {
                "rpc-access-reader"
            } else {
                "rpc-access-manager"
            };
            record_denial(
                &orm,
                "product.template",
                principal.subject().get(),
                login,
                Operation::Read,
                &erp_sdk::Error::access(error["message"].as_str().unwrap()),
            );
        }
        for (principal, method, args, op) in [
            (
                &reader,
                "write",
                serde_json::json!({"ids":[own],"vals":{"name":"forbidden"}}),
                Operation::Write,
            ),
            (
                &reader,
                "create",
                serde_json::json!({"vals_list":[{"name":"forbidden"}]}),
                Operation::Create,
            ),
            (
                &reader,
                "unlink",
                serde_json::json!({"ids":[own]}),
                Operation::Unlink,
            ),
            (
                &outsider,
                "read",
                serde_json::json!({"ids":[own],"fields":["name"],"load":null}),
                Operation::Read,
            ),
        ] {
            let (status, error) = rpc(&service, principal, method, args).await;
            assert_eq!(status, 403, "{error}");
            assert_eq!(error["name"], "erp.exceptions.AccessError");
            let groups = allowing_group_names(&orm, "product.template", op).await;
            model_denial(
                &orm,
                "product.template",
                op,
                &groups,
                &erp_sdk::Error::access(error["message"].as_str().unwrap()),
            );
        }
        let (status, created) = rpc(
            &service,
            &writer,
            "create",
            serde_json::json!({"vals_list":[{"name":"RPC manager created","company_id":main}]}),
        )
        .await;
        assert_eq!(status, 200, "{created}");
        let id = created[0].as_i64().unwrap();
        let (status, changed) = rpc(
            &service,
            &writer,
            "write",
            serde_json::json!({"ids":[id],"vals":{"name":"RPC manager changed"}}),
        )
        .await;
        assert_eq!(status, 200, "{changed}");
        let (status, read) = rpc(
            &service,
            &reader,
            "read",
            serde_json::json!({"ids":[id],"fields":["name"],"load":null}),
        )
        .await;
        assert_eq!(status, 200, "{read}");
        assert_eq!(read[0]["name"], "RPC manager changed");
        let (status, deleted) =
            rpc(&service, &writer, "unlink", serde_json::json!({"ids":[id]})).await;
        assert_eq!(status, 200, "{deleted}");
        let (status, own_read) = rpc(
            &service,
            &reader,
            "read",
            serde_json::json!({"ids":[own],"fields":["name"],"load":null}),
        )
        .await;
        assert_eq!(status, 200, "{own_read}");
        assert_eq!(own_read[0]["name"], "RPC own product");
    });
}

#[test]
fn catalog_access_contract_for_internal_manager_and_outsider() {
    product_privilege_and_light_groups_are_installed();
    legacy_product_upgrade_links_privilege_once_and_preserves_customization();
    product_access_migration_refuses_missing_or_mistyped_identity();
    settings_administrator_keeps_product_management_across_install_order();
    product_model_permissions_are_enforced_for_real_users();
    product_permissions_and_company_boundaries_hold_over_json2();
}

#[test]
fn catalog_company_contract_for_unrelated_companies_and_branch() {
    company_rules_cover_every_catalog_owner_as_real_users();
    product_permissions_and_company_boundaries_hold_over_json2();
}

/// unrelated company are not counted in the refusal)
#[test]
fn barcode_refusal_does_not_tell_how_many_hidden_holders_exist() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let a = xid(&installed, "base", "main_company").await;
        let manager = xid(&installed, "product", "group_product_manager").await;
        let system = xid(&installed, "base", "group_system").await;
        let unit = xid(&installed, "uom", "product_uom_unit").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        user(&mut setup, "barcode-probe-bootstrap", a, &[system]).await;
        let company_model = setup.env().records("res.company").unwrap();
        let currency = company_model
            .browse([a])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let mut others = Vec::new();
        for label in ["Barcode probe X", "Barcode holder Y"] {
            others.push(
                company_model
                    .create(
                        &mut setup,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str(label.into()))
                                .set_cell("currency_id", currency.clone()),
                        ],
                    )
                    .await
                    .unwrap()
                    .ids()[0],
            );
        }
        let (x, y) = (others[0], others[1]);
        let login = "barcode-probe-x-manager";
        let prober = user(&mut setup, login, x, &[manager]).await;
        // Two holders of each barcode, in companies the prober cannot read.
        for barcode in ["P6-SHARED", "P6-PACK"] {
            for company in [a, y] {
                setup
                    .env()
                    .records("product.template")
                    .unwrap()
                    .create(
                        &mut setup,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str(format!("{barcode} holder").into()))
                                .set_cell("company_id", Cell::Id(company))
                                .set_cell("barcode", Cell::Str(barcode.into())),
                        ],
                    )
                    .await
                    .unwrap();
            }
        }
        setup.commit().await.unwrap();
        drop(setup);
        let orm = secure_orm(&db, &installed).await;
        let hidden = erp_addon_product::barcode::HIDDEN_HOLDERS;

        // A shared product overlaps every company.
        let mut tx = orm.begin(UserId::new(prober).unwrap()).await.unwrap();
        let templates = tx.env().records("product.template").unwrap();
        let error = tx
            .savepoint(async |tx| {
                templates
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str("P6 shared probe".into()))
                                .set_cell("company_id", Cell::Null)
                                .set_cell("barcode", Cell::Str("P6-SHARED".into())),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
        assert_eq!(
            error.message(),
            format!(
                "These barcodes are already in use:\n\n- \"P6-SHARED\" is held by product: P6 shared probe, {hidden}\n\nHolders you have no access to are not named."
            )
        );
        tx.rollback().await.unwrap();
        drop(tx);

        // A shared packaging overlaps the hidden products too.
        let mut tx = orm.begin(UserId::new(prober).unwrap()).await.unwrap();
        let host = tx
            .env()
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("P6 packaging host".into()))
                        .set_cell("company_id", Cell::Null),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let host_variant: i64 = tx
            .query(
                "SELECT id FROM product_product WHERE product_tmpl_id=$1",
                &[&host],
            )
            .await
            .unwrap()[0]
            .get(0);
        let packagings = tx.env().records("product.uom").unwrap();
        let error = tx
            .savepoint(async |tx| {
                packagings
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("product_id", Cell::Id(host_variant))
                                .set_cell("uom_id", Cell::Id(unit))
                                .set_cell("company_id", Cell::Null)
                                .set_cell("barcode", Cell::Str("P6-PACK".into())),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
        let message = error.message();
        assert!(
            message.starts_with("These barcodes are already in use:"),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "{hidden}\n\nHolders you have no access to are not named."
            )),
            "{message}"
        );
        assert!(!message.contains("hidden record"), "{message}");
        tx.rollback().await.unwrap();
    });
}
