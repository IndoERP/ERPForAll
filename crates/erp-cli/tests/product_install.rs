//! The `product` addon installed into a real database: its data, security rows, views,
//! actions and menus load from the manifest, and company and branch scoping holds for a real user.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;
use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, UserId};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

const MODELS: [&str; 14] = [
    "product.category",
    "product.template",
    "product.product",
    "product.supplierinfo",
    "product.pricelist",
    "product.pricelist.item",
    "product.attribute",
    "product.attribute.value",
    "product.template.attribute.line",
    "product.template.attribute.value",
    "product.combo",
    "product.combo.item",
    "product.uom",
    "uom.uom",
];

fn system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
        ])
        .build()
        .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
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

async fn install(db: &TestDb) -> (ModuleSystem, Orm) {
    let system = system(db);
    let _init = system.initialize(InitOptions::new()).await.unwrap();
    let env = system.system_env(Context::new());
    let installed = system
        .button_immediate_install(&env, &[ModuleName::new("product").unwrap()])
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    (system, installed.orm)
}

async fn strings(orm: &Orm, sql: &str) -> Vec<String> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

/// after install; a denied write by a user is not asserted here)
/// `company_and_branch_scoping_for_a_real_user`)
/// name and the "General Information" page; the documents stat button and the image are not part
/// of this slice)
#[test]
fn product_installs_with_its_data_security_and_views() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;

        let categories = strings(
            &orm,
            "SELECT d.name || '=' || (c.name->>'en_US') FROM ir_model_data d \
             JOIN product_category c ON c.id = d.res_id \
             WHERE d.module='product' AND d.model='product.category' ORDER BY d.name",
        )
        .await;
        assert_eq!(
            categories,
            [
                "product_category_expenses=Expenses",
                "product_category_goods=Goods",
                "product_category_services=Services"
            ]
        );

        let precisions = strings(
            &orm,
            "SELECT name || '=' || digits::text FROM decimal_precision \
             WHERE name IN ('Product Price','Discount','Stock Weight','Volume') ORDER BY name",
        )
        .await;
        assert_eq!(
            precisions,
            [
                "Discount=2",
                "Product Price=2",
                "Stock Weight=2",
                "Volume=2"
            ]
        );

        let attributes = strings(
            &orm,
            "SELECT d.name || '=' || (a.name->>'en_US') || '/' || a.display_type || '/' || a.create_variant \
             FROM ir_model_data d JOIN product_attribute a ON a.id = d.res_id \
             WHERE d.module='product' AND d.model='product.attribute' ORDER BY a.sequence",
        )
        .await;
        assert_eq!(
            attributes,
            [
                "pa_brand=Brand/radio/always",
                "pa_size=Size/pills/always",
                "pa_color=Color/color/always",
                "pa_material=Material/radio/always",
                "pa_pattern=Pattern/radio/always"
            ]
        );

        for model in MODELS {
            let rows = strings(
                &orm,
                &format!(
                    "SELECT g.module || '/' || a.operation FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
                     JOIN ir_model_data g ON g.model='res.groups' AND g.res_id = a.group_id \
                     WHERE m.model = '{model}' AND ((g.module='base' AND g.name='group_user') \
                        OR (g.module='product' AND g.name='group_product_manager')) \
                     ORDER BY a.operation"
                ),
            )
            .await;
            assert!(
                rows == ["product/crud", "base/r"] || rows == ["base/r", "product/crud"],
                "{model}: user read and manager crud rows missing, found {rows:?}"
            );
        }

        let rules = strings(
            &orm,
            "SELECT d.name FROM ir_model_data d WHERE d.module='product' AND d.name LIKE '%comp_rule' ORDER BY d.name",
        )
        .await;
        assert_eq!(rules.len(), 5, "company rules: {rules:?}");

        let actions = strings(
            &orm,
            "SELECT name FROM ir_model_data WHERE module='product' AND model='ir.actions.act_window' \
             AND name IN ('product_template_action','product_product_action','product_pricelist_action2', \
                          'product_category_action_form','product_uom_action_view_list', \
                          'product_attribute_action') ORDER BY name",
        )
        .await;
        assert_eq!(actions.len(), 6, "actions: {actions:?}");
        let menus = strings(
            &orm,
            "SELECT name FROM ir_model_data WHERE module='product' AND model='ir.ui.menu' ORDER BY name",
        )
        .await;
        assert_eq!(menus.len(), 6, "menus: {menus:?}");

        let arch = strings(
            &orm,
            "SELECT arch_db::text FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id \
             WHERE d.module='product' AND d.name='product_template_form_view'",
        )
        .await;
        let arch = &arch[0];
        for needle in ["button_box", "General Information", "name=\"name\""] {
            assert!(arch.contains(needle), "form arch lacks {needle}");
        }

        // the product group exists and the default unit is the one a new product takes
        let unit = strings(
            &orm,
            "SELECT name FROM ir_model_data WHERE module='uom' AND name='product_uom_unit' AND model='uom.uom'",
        )
        .await;
        assert_eq!(unit, ["product_uom_unit"]);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let tmpl = tx
            .env()
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Installed".into()))],
            )
            .await
            .unwrap();
        let uom = tmpl.cell(&mut tx, "uom_id").await.unwrap();
        let expected = tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='uom' AND name='product_uom_unit'",
                &[],
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        assert_eq!(uom, Cell::Id(expected));
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

async fn xid(orm: &Orm, module: &str, name: &str) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let id = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module=$1 AND name=$2",
            &[&module, &name],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    tx.rollback().await.unwrap();
    id
}

/// models share the generated rule), (partial: same barcode in disjoint companies through a real
/// user) and the barcode error of a hidden product (lead security finding).
#[test]
fn company_and_branch_scoping_for_a_real_user() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(Resolver::for_registry(installed.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let main = xid(&orm, "base", "main_company").await;
        let manager_group = xid(&orm, "product", "group_product_manager").await;

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let companies = tx.env().records("res.company").unwrap();
        let main_rec = companies.browse([main]).unwrap();
        let currency = main_rec.cell(&mut tx, "currency_id").await.unwrap();
        let branch = companies
            .with_context([("default_parent_id", erp_sdk::Value::Int(main))])
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Branch B".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let other = companies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Company X".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let templates = tx.env().records("product.template").unwrap();
        let mut make = async |name: &str, company: Option<i64>, barcode: Option<&str>| {
            let mut vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
            if let Some(c) = company {
                vals = vals.set_cell("company_id", Cell::Id(c));
            }
            if let Some(b) = barcode {
                vals = vals.set_cell("barcode", Cell::Str(b.into()));
            }
            templates.create(&mut tx, vec![vals]).await.unwrap()
        };
        make("Parent product", Some(main), None).await;
        make("Branch product", Some(branch), None).await;
        make("Shared product", None, None).await;
        make("Secret X Product", Some(other), Some("SECRET-X")).await;
        // one pricelist rule under the parent company and one under Company X
        let pricelists = tx.env().records("product.pricelist").unwrap();
        let items = tx.env().records("product.pricelist.item").unwrap();
        let mut pricelist_with_item = async |name: &str, company: i64| {
            let pl = pricelists
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("company_id", Cell::Id(company)),
                    ],
                )
                .await
                .unwrap()
                .ids()[0];
            items
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("pricelist_id", Cell::Id(pl))
                            .set_cell("applied_on", Cell::Str("3_global".into())),
                    ],
                )
                .await
                .unwrap()
                .ids()[0]
        };
        let parent_item = pricelist_with_item("Parent pricelist", main).await;
        pricelist_with_item("Company X pricelist", other).await;
        let users = tx.env().records("res.users").unwrap();
        // the base module requires an administrator user to exist before any other user
        let system_group = xid(&orm, "base", "group_system").await;
        users
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("product-admin".into()))
                        .set_cell("login", Cell::Str("product-admin".into()))
                        .set_cell(
                            "password",
                            Cell::Str("product-admin-fixture-password".into()),
                        )
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(system_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user = users
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("branch-user".into()))
                        .set_cell("login", Cell::Str("branch-user".into()))
                        .set_cell("password", Cell::Str("branch-user-fixture-password".into()))
                        .set_cell("company_id", Cell::Id(branch))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(branch).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(manager_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let shared = templates
            .search(
                &mut tx,
                &erp_sdk::Domain::condition("name", erp_sdk::Operator::Eq, "Shared product")
                    .unwrap(),
            )
            .await
            .unwrap()
            .ids()[0];
        let shared_variant = tx
            .env()
            .records("product.product")
            .unwrap()
            .search(
                &mut tx,
                &erp_sdk::Domain::condition("product_tmpl_id", erp_sdk::Operator::Eq, shared)
                    .unwrap(),
            )
            .await
            .unwrap()
            .ids()[0];
        let vendor = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Shared vendor".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut supplier_ids = Vec::new();
        for company in [Some(main), Some(branch), Some(other), None] {
            let suppliers = tx
                .env()
                .with_company(main)
                .unwrap()
                .records("product.supplierinfo")
                .unwrap();
            let row = suppliers
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("partner_id", Cell::Id(vendor))
                            .set_cell("product_id", Cell::Id(shared_variant))
                            .set_cell(
                                "company_id",
                                match company {
                                    Some(id) => Cell::Id(id),
                                    None => Cell::Null,
                                },
                            )
                            .set_cell("price", Cell::Decimal(erp_sdk::Decimal::ONE)),
                    ],
                )
                .await
                .unwrap();
            supplier_ids.push(row.ids()[0]);
        }
        tx.commit().await.unwrap();
        drop(tx);

        let mut utx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
        let visible = utx.env().records("product.template").unwrap();
        let found = visible
            .search(&mut utx, &erp_sdk::Domain::constant(true))
            .await
            .unwrap();
        let mut names = Vec::new();
        for id in found.ids() {
            let rec = visible.browse([id]).unwrap();
            if let Cell::Str(n) = rec.cell(&mut utx, "name").await.unwrap() {
                names.push(n.to_string());
            }
        }
        names.sort();
        assert_eq!(
            names,
            ["Branch product", "Parent product", "Shared product"],
            "a branch user sees its own company, the parent company and shared products, not another company"
        );

        // pricelist rules follow their pricelist's company (N-M1: no rule on a missing field)
        let visible_items = utx
            .env()
            .records("product.pricelist.item")
            .unwrap()
            .search(&mut utx, &erp_sdk::Domain::constant(true))
            .await
            .unwrap();
        assert_eq!(
            visible_items.ids(),
            vec![parent_item],
            "a branch user reads the parent company's pricelist rules, not another company's"
        );
        let product = utx
            .env()
            .records("product.product")
            .unwrap()
            .browse([shared_variant])
            .unwrap();
        let eligible: erp_orm::Records = product
            .call(
                &mut utx,
                "_get_filtered_sellers",
                (erp_addon_product::SellerQuery::default().with_quantity(None),),
            )
            .await
            .unwrap();
        assert_eq!(
            eligible.ids(),
            vec![supplier_ids[1], supplier_ids[3]],
            "a real branch user selects branch/shared rows, excluding readable ancestor rows and hidden unrelated rows"
        );

        // same barcode in a disjoint company is allowed
        let ok = visible
            .create(
                &mut utx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Branch twin".into()))
                        .set_cell("company_id", Cell::Id(branch))
                        .set_cell("barcode", Cell::Str("SECRET-X".into())),
                ],
            )
            .await;
        assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.message().to_owned()));
        utx.rollback().await.unwrap();

        // a shared product collides with every company: the error must not name the hidden product
        let mut utx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
        let visible = utx.env().records("product.template").unwrap();
        let err = visible
            .create(
                &mut utx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Shared twin".into()))
                        .set_cell("barcode", Cell::Str("SECRET-X".into())),
                ],
            )
            .await
            .expect_err("the barcode is used by a product of another company");
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert!(
            !err.message().contains("Secret X Product"),
            "the error discloses a product the user cannot read: {}",
            err.message()
        );
        assert!(err.message().contains("SECRET-X"), "{}", err.message());
        utx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

/// Installed-schema upgrade exercise; legacy shape/version are simulated explicitly.
/// This does not claim an old-binary install was executed. The last case upgrades `base`, `product`
/// and `web` in one run, as a deployment does: `base`'s schema step comes first and must leave the
/// vendor table to `product`'s step, after its pre-migration filled the new required unit.
/// changes its table, also when `base` is upgraded with it)
#[test]
fn supplier_unit_upgrade_preserves_terms_and_refuses_invalid_sources() {
    big_stack(async {
        for (invalid, modules) in [
            (false, &["product"][..]),
            (true, &["product"][..]),
            (false, &["base", "product", "web"][..]),
        ] {
            let db = TestDb::create().await.unwrap();
            let (system, orm) = install(&db).await;
            let main = xid(&orm, "base", "main_company").await;
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let env = tx.env().with_company(main).unwrap();
            let vendor = env
                .records("res.partner")
                .unwrap()
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("Upgrade vendor".into()))],
                )
                .await
                .unwrap()
                .ids()[0];
            let template = env
                .records("product.template")
                .unwrap()
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("Upgrade part".into()))],
                )
                .await
                .unwrap();
            let template_id = template.ids()[0];
            let unit = template.cell(&mut tx, "uom_id").await.unwrap();
            let row = env
                .records("product.supplierinfo")
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("partner_id", Cell::Id(vendor))
                            .set_cell("product_tmpl_id", Cell::Id(template_id))
                            .set_cell("company_id", Cell::Null)
                            .set_cell("price", Cell::Decimal(erp_sdk::Decimal::new(1234, 2))),
                    ],
                )
                .await
                .unwrap()
                .ids()[0];
            tx.flush().await.unwrap();
            tx.batch(
                "ALTER TABLE product_supplierinfo DROP COLUMN uom_id;
                ALTER TABLE product_supplierinfo ALTER COLUMN min_qty DROP NOT NULL;
                UPDATE product_supplierinfo SET min_qty = NULL;
                UPDATE ir_module_module SET latest_version = '1.0.0' WHERE name = 'product';",
            )
            .await
            .unwrap();
            if invalid {
                tx.batch("ALTER TABLE product_template ALTER COLUMN uom_id DROP NOT NULL")
                    .await
                    .unwrap();
                tx.execute(
                    "UPDATE product_template SET uom_id = NULL WHERE id=$1",
                    &[&template_id],
                )
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
            let env = system.system_env(Context::new());
            let result = system
                .button_immediate_upgrade(
                    &env,
                    &modules
                        .iter()
                        .map(|m| ModuleName::new(*m).unwrap())
                        .collect::<Vec<_>>(),
                )
                .await;
            if invalid {
                let error = match result {
                    Err(error) => error,
                    Ok(_) => panic!("bad legacy product link must refuse upgrade"),
                };
                let error = match error {
                    erp_modules::Error::StepFailed { source, .. } => *source,
                    other => other,
                };
                match error {
                    erp_modules::Error::MigrationFailed { source, .. } => {
                        assert_eq!(source.kind(), erp_sdk::ErrorKind::Validation);
                        assert_eq!(
                            source.message(),
                            format!(
                                "Vendor unit migration refused 1 supplier rows with missing or inconsistent product/unit links; first IDs: {row}."
                            )
                        );
                    }
                    other => panic!("unexpected upgrade error: {other:?}"),
                }
                let client = db.pool().get().await.unwrap();
                let column: bool = client.query_one("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='product_supplierinfo'::regclass AND attname='uom_id' AND NOT attisdropped)",&[]).await.unwrap().get(0);
                assert!(
                    !column,
                    "failed lifecycle must roll back the added unit column"
                );
                let version: String = client
                    .query_one(
                        "SELECT latest_version FROM ir_module_module WHERE name='product'",
                        &[],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(version, "1.0.0");
            } else {
                let upgraded = result.unwrap_or_else(|e| panic!("upgrade of {modules:?}: {e:?}"));
                assert!(
                    upgraded.post_commit.is_clean(),
                    "{:?}",
                    upgraded.post_commit
                );
                let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
                let row = tx
                    .env()
                    .records("product.supplierinfo")
                    .unwrap()
                    .browse([row])
                    .unwrap();
                assert_eq!(row.cell(&mut tx, "uom_id").await.unwrap(), unit);
                assert_eq!(row.cell(&mut tx, "company_id").await.unwrap(), Cell::Null);
                assert_eq!(
                    row.cell(&mut tx, "price").await.unwrap(),
                    Cell::Decimal(erp_sdk::Decimal::new(1234, 2))
                );
                assert_eq!(
                    row.cell(&mut tx, "min_qty").await.unwrap(),
                    Cell::Decimal(erp_sdk::Decimal::ZERO)
                );
                tx.rollback().await.unwrap();
                drop(tx);
                drop(upgraded);
            }
            drop(system);
            drop(orm);
            db.cleanup().await.unwrap();
        }
    });
}

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

/// Every primary view the addon ships is served by the web client's `get_views` to a real product manager,
/// on desktop and phone (lesson: a view attribute the web client does not support reached the
/// dev/testing deployment because the install test read the arch text only).
#[test]
fn every_product_view_is_served_to_a_product_manager() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
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
            .unwrap();
        let main = xid(&orm, "base", "main_company").await;
        let system_group = xid(&orm, "base", "group_system").await;
        let manager_group = xid(&orm, "product", "group_product_manager").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let users = tx.env().records("res.users").unwrap();
        for (login, group) in [
            ("views-admin", system_group),
            ("views-manager", manager_group),
        ] {
            users
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(login.into()))
                            .set_cell("login", Cell::Str(login.into()))
                            .set_cell(
                                "password",
                                Cell::Str(format!("{login}-fixture-password").into()),
                            )
                            .set_cell("company_id", Cell::Id(main))
                            .set_commands(
                                "company_ids",
                                [Command::Set(vec![RecId::from_real(main).unwrap()])],
                            )
                            .set_commands(
                                "group_ids",
                                [Command::Set(vec![RecId::from_real(group).unwrap()])],
                            ),
                    ],
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        drop(tx);

        let auth = Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap();
        let logged = auth
            .authenticate(LoginRequest::new(
                "views-manager",
                "views-manager-fixture-password",
                ClientAddr::InProcess("product-views-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        begun.tx.rollback().await.unwrap();

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT v.id, v.model, v.type FROM ir_ui_view v JOIN ir_model_data d \
                 ON d.model = 'ir.ui.view' AND d.res_id = v.id \
                 WHERE d.module = 'product' AND v.inherit_id IS NULL ORDER BY d.name",
                &[],
            )
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let views: Vec<(i64, String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, i64>(0),
                    r.get::<_, String>(1),
                    r.get::<_, String>(2),
                )
            })
            .collect();
        assert!(views.len() >= 10, "product views: {views:?}");

        // onchange is not a read: the audit store must exist before the call
        {
            let mut audit_tx = orm
                .begin_opts(
                    erp_orm::BeginOptions::new(SUPERUSER_ID)
                        .isolation(erp_orm::Isolation::ReadCommitted)
                        .on_primary(),
                )
                .await
                .unwrap();
            audit_tx.install_audit_schema().await.unwrap();
            audit_tx.commit().await.unwrap();
        }
        let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(main).unwrap();
        let vendor = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("RPC vendor".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut templates = Vec::new();
        for name in ["RPC supplier part", "RPC other part"] {
            templates.push(
                env.records("product.template")
                    .unwrap()
                    .create(
                        &mut tx,
                        vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
                    )
                    .await
                    .unwrap()
                    .ids()[0],
            );
        }
        let variant = env
            .records("product.product")
            .unwrap()
            .search(
                &mut tx,
                &erp_sdk::Domain::condition("product_tmpl_id", erp_sdk::Operator::Eq, templates[0])
                    .unwrap(),
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        let response = service.call(RpcCall::json2("/json/2/product.supplierinfo/create",
            serde_json::to_vec(&serde_json::json!({"vals_list":[{"partner_id":vendor,"product_id":variant,"price":"100","discount":"15","min_qty":"1.25"}]})).unwrap(),
            Caller::Principal(principal.clone()))).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let created: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        let supplier = created[0].as_i64().unwrap();
        let response = service.call(RpcCall::json2("/json/2/product.supplierinfo/read",
            serde_json::to_vec(&serde_json::json!({"ids":[supplier],"load":null,"fields":["product_tmpl_id","company_id","price_discounted","min_qty"]})).unwrap(),
            Caller::Principal(principal.clone()))).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let read: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(read[0]["product_tmpl_id"], templates[0]);
        assert_eq!(read[0]["company_id"], main);
        assert_eq!(read[0]["price_discounted"], 85);
        assert_eq!(read[0]["min_qty"].to_string(), "1.25");
        let response = service.call(RpcCall::json2("/json/2/product.supplierinfo/onchange",
            serde_json::to_vec(&serde_json::json!({"ids":[supplier],"values":{"partner_id":vendor,"product_id":variant,"product_tmpl_id":templates[1]},"field_names":["product_tmpl_id"],"fields_spec":{"product_id":{},"product_tmpl_id":{},"uom_id":{}}})).unwrap(),
            Caller::Principal(principal.clone()))).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let onchange: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(onchange["value"]["product_id"], false, "{onchange}");
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let stored = tx
            .env()
            .records("product.supplierinfo")
            .unwrap()
            .browse([supplier])
            .unwrap();
        assert_eq!(
            stored.cell(&mut tx, "product_id").await.unwrap(),
            Cell::Id(variant)
        );
        assert_eq!(
            stored.cell(&mut tx, "product_tmpl_id").await.unwrap(),
            Cell::Id(templates[0])
        );
        tx.rollback().await.unwrap();
        let mut refused = Vec::new();
        for (id, model, kind) in &views {
            for mobile in [false, true] {
                let response = service
                    .call(RpcCall::json2(
                        &format!("/json/2/{model}/get_views"),
                        serde_json::to_vec(&serde_json::json!({
                            "views": [[id, kind]], "wire_major": 1, "options": {"mobile": mobile}
                        }))
                        .unwrap(),
                        Caller::Principal(principal.clone()),
                    ))
                    .await;
                if response.status() != 200 {
                    refused.push(format!(
                        "{model} {kind} view {id} mobile={mobile}: {} {}",
                        response.status(),
                        String::from_utf8_lossy(response.body())
                    ));
                }
            }
        }
        assert!(
            refused.is_empty(),
            "views the web client refuses:\n{}",
            refused.join("\n")
        );

        // A one2many or many2many shown as a sub-list in a product form takes its columns from
        // the target model's list view, which the client asks for separately: each one must be
        // served too (the attribute lines of the product form had none).
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let sub_lists = tx
            .query(
                "SELECT f.model, f.name, f.relation, v.arch_db FROM ir_ui_view v \
                 JOIN ir_model_data d ON d.model = 'ir.ui.view' AND d.res_id = v.id \
                 JOIN ir_model_fields f ON f.model = v.model \
                 AND f.ttype IN ('one2many', 'many2many') \
                 WHERE d.module = 'product' AND v.type = 'form' ORDER BY f.model, f.name",
                &[],
            )
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let mut checked = Vec::new();
        for row in &sub_lists {
            let (model, field, relation, arch): (String, String, String, String) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            let tag = format!("<field name=\"{field}\"");
            let Some(start) = arch.find(&tag) else {
                continue;
            };
            let node = &arch[start..start + arch[start..].find('>').unwrap()];
            if node.contains("widget=") {
                continue; // tags, counters and other widgets do not render a list
            }
            checked.push(format!("{model}.{field}"));
            let response = service
                .call(RpcCall::json2(
                    &format!("/json/2/{relation}/get_views"),
                    serde_json::to_vec(&serde_json::json!({
                        "views": [[null, "list"]], "wire_major": 1, "options": {}
                    }))
                    .unwrap(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            if response.status() != 200 {
                refused.push(format!(
                    "{model}.{field} list of {relation}: {} {}",
                    response.status(),
                    String::from_utf8_lossy(response.body())
                ));
            }
        }
        assert!(
            checked
                .iter()
                .any(|f| f == "product.template.attribute_line_ids"),
            "the attribute lines sub-list is checked: {checked:?}"
        );
        assert!(
            refused.is_empty(),
            "sub-lists without a list view:\n{}",
            refused.join("\n")
        );

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let template = tx
            .env()
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Variants button".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        drop(tx);
        let opened = service
            .call(RpcCall::json2(
                "/json/2/product.template/action_open_variants",
                serde_json::to_vec(&serde_json::json!({"ids": [template]})).unwrap(),
                Caller::Principal(principal.clone()),
            ))
            .await;
        assert_eq!(
            opened.status(),
            200,
            "{}",
            String::from_utf8_lossy(opened.body())
        );
        let action: serde_json::Value = serde_json::from_slice(opened.body()).unwrap();
        let normalized = service
            .call(RpcCall::json2(
                "/json/2/product.template/erp_normalize_action_result",
                serde_json::to_vec(&serde_json::json!({"wire_major": 1, "result": action}))
                    .unwrap(),
                Caller::Principal(principal.clone()),
            ))
            .await;
        assert_eq!(
            normalized.status(),
            200,
            "the web client refuses the Variants action: {}",
            String::from_utf8_lossy(normalized.body())
        );
        let normalized: serde_json::Value = serde_json::from_slice(normalized.body()).unwrap();
        assert_eq!(
            normalized["result"]["model"],
            serde_json::json!("product.product"),
            "{normalized}"
        );
        assert_eq!(
            normalized["result"]["context"],
            serde_json::json!({"create": false, "default_product_tmpl_id": template}),
            "{normalized}"
        );

        // The variant list reads its value tags with the field's `display_attribute_name: False`
        // context (as the web client now sends it): "Black", not "Color: Black".
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().clone();
        let color = env
            .records("product.attribute")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Color".into()))],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let mut values: Vec<RecId> = Vec::new();
        for name in ["Black", "White"] {
            values.push(
                env.records("product.attribute.value")
                    .unwrap()
                    .create(
                        &mut tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str(name.into()))
                                .set_cell("attribute_id", Cell::Id(color)),
                        ],
                    )
                    .await
                    .unwrap()
                    .rec_ids()[0],
            );
        }
        let shirt = env
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Tag shirt".into()))],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        env.records("product.template.attribute.line")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("product_tmpl_id", Cell::Id(shirt))
                        .set_cell("attribute_id", Cell::Id(color))
                        .set_cell("value_ids", Cell::Ids(values.into())),
                ],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let tags = |context: Option<serde_json::Value>| {
            let mut item = serde_json::json!({"fields": {"display_name": {}}});
            if let Some(context) = context {
                item["context"] = context;
            }
            serde_json::json!({
                "domain": [["product_tmpl_id", "=", shirt]],
                "specification": {"product_template_attribute_value_ids": item},
                "order": "id asc",
            })
        };
        let mut names = Vec::new();
        for context in [
            Some(serde_json::json!({"display_attribute_name": false})),
            None,
        ] {
            let read = service
                .call(RpcCall::json2(
                    "/json/2/product.product/web_search_read",
                    serde_json::to_vec(&tags(context)).unwrap(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            assert_eq!(
                read.status(),
                200,
                "{}",
                String::from_utf8_lossy(read.body())
            );
            let read: serde_json::Value = serde_json::from_slice(read.body()).unwrap();
            let mut shown: Vec<String> = read["records"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|row| {
                    row["product_template_attribute_value_ids"]
                        .as_array()
                        .unwrap()
                        .clone()
                })
                .map(|tag| tag["display_name"].as_str().unwrap().to_owned())
                .collect();
            shown.sort();
            names.push(shown);
        }
        assert_eq!(
            names[0],
            ["Black", "White"],
            "with the view's field context"
        );
        assert_eq!(names[1], ["Color: Black", "Color: White"], "without it");

        // the variant's computes run on a record that has no database id yet.
        let relation = serde_json::json!({"fields": {"display_name": {}}});
        let onchange = serde_json::json!({
            "ids": [],
            "values": {},
            "field_names": [],
            "fields_spec": {
                "name": {}, "product_tmpl_id": relation, "default_code": {}, "barcode": {},
                "lst_price": {}, "price_extra": {}, "standard_price": {}, "uom_id": relation,
                "active": {},
                "product_template_attribute_value_ids": {
                    "fields": {"display_name": {}},
                    "context": {"display_attribute_name": false},
                },
            },
        });
        let changed = service
            .call(RpcCall::json2(
                "/json/2/product.product/onchange",
                serde_json::to_vec(&onchange).unwrap(),
                Caller::Principal(principal.clone()),
            ))
            .await;
        let body = String::from_utf8_lossy(changed.body()).into_owned();
        assert_eq!(changed.status(), 200, "{body}");
        let changed: serde_json::Value = serde_json::from_slice(changed.body()).unwrap();
        assert!(changed["value"].is_object(), "{changed}");

        // finds through a menu whose window action has a form view for `product.attribute`.
        let routed = strings(
            &orm,
            "SELECT a.res_model || ':' || a.view_mode FROM ir_ui_menu m \
             JOIN ir_model_data d ON d.model = 'ir.ui.menu' AND d.res_id = m.id \
             JOIN ir_act_window a ON m.action = 'ir.actions.act_window,' || a.id \
             WHERE d.module = 'product' AND d.name = 'menu_product_attribute'",
        )
        .await;
        assert_eq!(routed, ["product.attribute:list,form"]);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn product_and_stock_install_together() {
    big_stack(async {
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
            .view_types(erp_addon_web::view_type_contributions())
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .code([
                &erp_addon_base::ADDON,
                &erp_addon_web::ADDON,
                &erp_addon_uom::ADDON,
                &erp_addon_product::ADDON,
                &erp_addon_barcodes::ADDON,
                &erp_addon_barcodes_gs1_nomenclature::ADDON,
                &erp_addon_stock::ADDON,
            ])
            .build()
            .unwrap();
        let _init = system.initialize(InitOptions::new()).await.unwrap();
        let env = system.system_env(Context::new());
        let installed = system
            .button_immediate_install(&env, &[ModuleName::new("stock").unwrap()])
            .await
            .unwrap();
        assert!(
            installed.post_commit.is_clean(),
            "{:?}",
            installed.post_commit
        );
        let states = strings(
            &installed.orm,
            "SELECT name || '=' || state FROM ir_module_module \
             WHERE name IN ('uom','product','barcodes','barcodes_gs1_nomenclature','stock') ORDER BY name",
        )
        .await;
        assert_eq!(
            states,
            [
                "barcodes=installed",
                "barcodes_gs1_nomenclature=installed",
                "product=installed",
                "stock=installed",
                "uom=installed"
            ]
        );
        let tables = strings(
            &installed.orm,
            "SELECT table_name FROM information_schema.tables WHERE table_schema='public' \
             AND table_name IN ('product_template','product_product','product_uom') ORDER BY table_name",
        )
        .await;
        assert_eq!(
            tables,
            ["product_product", "product_template", "product_uom"]
        );
        db.cleanup().await.unwrap();
    });
}

#[test]
fn installed_product_labels_respect_supplier_acl_on_rpc_and_relations() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let main = xid(&installed, "base", "main_company").await;
        let group = xid(&installed, "base", "group_user").await;
        let unit = xid(&installed, "uom", "product_uom_unit").await;
        let mut seed = installed.begin(SUPERUSER_ID).await.unwrap();
        let env = seed.env().with_company(main).unwrap();
        let currency = env
            .records("res.company")
            .unwrap()
            .browse([main])
            .unwrap()
            .cell(&mut seed, "currency_id")
            .await
            .unwrap();
        let template = env
            .records("product.template")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Public chair".into()))
                        .set_cell("default_code", Cell::Str("PUBLIC".into())),
                ],
            )
            .await
            .unwrap();
        let variant = env
            .records("product.product")
            .unwrap()
            .search(
                &mut seed,
                &erp_sdk::Domain::condition(
                    "product_tmpl_id",
                    erp_sdk::Operator::Eq,
                    template.ids()[0],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let vendor = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut seed,
                vec![RawVals::new().set_cell("name", Cell::Str("Label vendor".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        env.records("product.supplierinfo")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(vendor))
                        .set_cell("product_tmpl_id", Cell::Id(template.ids()[0]))
                        .set_cell("product_id", Cell::Id(variant.ids()[0]))
                        .set_cell("currency_id", currency)
                        .set_cell("company_id", Cell::Id(main))
                        .set_cell("product_code", Cell::Str("PRIVATE".into()))
                        .set_cell(
                            "product_name",
                            Cell::Str("Private supplier description".into()),
                        ),
                ],
            )
            .await
            .unwrap();
        let packaging = env
            .records("product.uom")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("product_id", Cell::Id(variant.ids()[0]))
                        .set_cell("uom_id", Cell::Id(unit))
                        .set_cell("barcode", Cell::Str("LABEL-HOLDER".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let admin_group = xid(&installed, "base", "group_system").await;
        env.records("res.users")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Label admin".into()))
                        .set_cell("login", Cell::Str("label-admin".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(admin_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user = env
            .records("res.users")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Label reader".into()))
                        .set_cell("login", Cell::Str("label-reader".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        seed.commit().await.unwrap();
        drop(seed);
        let allowed_orm = product_policy_orm(&installed, &db).await;
        let mut switched = allowed_orm.begin(SUPERUSER_ID).await.unwrap();
        let switched_env = switched
            .env()
            .with_user(user)
            .with_context([("partner_id", erp_sdk::Value::Int(vendor))]);
        assert!(
            switched_env.companies().unwrap().is_empty(),
            "fixture must use an unresolved switch without explicit companies"
        );
        assert_eq!(
            switched
                .companies(&switched_env)
                .await
                .unwrap()
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            [main]
        );
        let visible = switched_env
            .records("product.product")
            .unwrap()
            .browse(variant.ids().iter().copied())
            .unwrap();
        assert_eq!(
            visible.cell(&mut switched, "display_name").await.unwrap(),
            Cell::Str("[PRIVATE] Private supplier description".into())
        );
        switched.rollback().await.unwrap();
        drop(switched);
        drop(allowed_orm);
        // Remove actual installed read ACLs, rather than assuming the base user lacks them.
        let mut seed = installed.begin(SUPERUSER_ID).await.unwrap();
        seed.execute("DELETE FROM ir_access WHERE model_id IN (SELECT id FROM ir_model WHERE model = 'product.supplierinfo') AND operation = 'r'", &[]).await.unwrap();
        seed.commit().await.unwrap();
        drop(seed);
        let orm = product_policy_orm(&installed, &db).await;
        let mut tx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
        assert!(
            !tx.env()
                .records("product.supplierinfo")
                .unwrap()
                .has_access(&mut tx, erp_orm::Operation::Read)
                .await
                .unwrap()
        );
        let reader = tx
            .env()
            .clone()
            .with_context([("partner_id", erp_sdk::Value::Int(vendor))]);
        let product = reader
            .records("product.product")
            .unwrap()
            .browse(variant.ids().iter().copied())
            .unwrap();
        for (field, expected) in [
            ("display_name", "[PUBLIC] Public chair"),
            ("code", "PUBLIC"),
            ("partner_ref", "[PUBLIC] Public chair"),
        ] {
            assert_eq!(
                product.cell(&mut tx, field).await.unwrap(),
                Cell::Str(expected.into())
            );
        }
        let linked = reader
            .records("product.uom")
            .unwrap()
            .browse([packaging])
            .unwrap()
            .read(&mut tx, &["product_id"])
            .await
            .unwrap();
        assert_eq!(
            linked[0]["product_id"],
            erp_sdk::Value::list([
                erp_sdk::Value::Int(variant.ids()[0]),
                erp_sdk::Value::str("[PUBLIC] Public chair")
            ])
        );
        let principal = tx.env().principal().clone();
        tx.rollback().await.unwrap();
        drop(tx);
        let service = RpcService::new(orm, Host, RpcConfig::default());
        let response = service.call(RpcCall::json2("/json/2/product.product/read",
            serde_json::to_vec(&serde_json::json!({"ids": [variant.ids()[0]], "fields": ["display_name", "code", "partner_ref"], "context": {"partner_id": vendor}})).unwrap(), Caller::Principal(principal))).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let result: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(
            result[0]["display_name"], "[PUBLIC] Public chair",
            "{result}"
        );
        assert_eq!(result[0]["code"], "PUBLIC", "{result}");
        assert_eq!(
            result[0]["partner_ref"], "[PUBLIC] Public chair",
            "{result}"
        );
        db.cleanup().await.unwrap();
    });
}

/// the label its reader may see)
///
/// A debugging internal user (`base.group_no_one`) is denied a product by a record rule while
/// `partner_id` of a vendor with a supplier row is in the context: the error lists the product by
/// the vendor label while the user may read supplier rows, and by the public label once that read
/// access is removed (cycle 12 review round 3, M1: the listing read the label as superuser).
#[test]
fn installed_access_error_lists_a_product_by_the_label_its_reader_may_see() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let main = xid(&installed, "base", "main_company").await;
        let internal = xid(&installed, "base", "group_user").await;
        let mut seed = installed.begin(SUPERUSER_ID).await.unwrap();
        let env = seed.env().with_company(main).unwrap();
        // the install does not seed `base.group_no_one`: name a group so before the policy compiles
        let no_one = env
            .records("res.groups")
            .unwrap()
            .create(
                &mut seed,
                vec![RawVals::new().set_cell("name", Cell::Str("Technical features".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        env.records("ir.model.data")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("module", Cell::Str("base".into()))
                        .set_cell("name", Cell::Str("group_no_one".into()))
                        .set_cell("model", Cell::Str("res.groups".into()))
                        .set_cell("res_id", Cell::Id(no_one))
                        .set_cell("noupdate", Cell::Bool(true)),
                ],
            )
            .await
            .unwrap();
        let currency = env
            .records("res.company")
            .unwrap()
            .browse([main])
            .unwrap()
            .cell(&mut seed, "currency_id")
            .await
            .unwrap();
        let template = env
            .records("product.template")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Public chair".into()))
                        .set_cell("default_code", Cell::Str("PUBLIC".into())),
                ],
            )
            .await
            .unwrap();
        let variant = env
            .records("product.product")
            .unwrap()
            .search(
                &mut seed,
                &erp_sdk::Domain::condition(
                    "product_tmpl_id",
                    erp_sdk::Operator::Eq,
                    template.ids()[0],
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .ids()[0];
        let vendor = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut seed,
                vec![RawVals::new().set_cell("name", Cell::Str("Label vendor".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        env.records("product.supplierinfo")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(vendor))
                        .set_cell("product_tmpl_id", Cell::Id(template.ids()[0]))
                        .set_cell("product_id", Cell::Id(variant))
                        .set_cell("currency_id", currency)
                        .set_cell("company_id", Cell::Id(main))
                        .set_cell("product_code", Cell::Str("PRIVATE".into()))
                        .set_cell(
                            "product_name",
                            Cell::Str("Private supplier description".into()),
                        ),
                ],
            )
            .await
            .unwrap();
        // a user write needs an administrator to remain (as in the test above)
        let admin_group = xid(&installed, "base", "group_system").await;
        env.records("res.users")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Debug admin".into()))
                        .set_cell("login", Cell::Str("debug-admin".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(admin_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user = env
            .records("res.users")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Debug reader".into()))
                        .set_cell("login", Cell::Str("debug-reader".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![
                                RecId::from_real(internal).unwrap(),
                                RecId::from_real(no_one).unwrap(),
                            ])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        // a global record rule that hides this one product from read
        seed.execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
             SELECT 'hide the label product', id, NULL, 'r', $1, true FROM ir_model \
             WHERE model = 'product.product'",
            &[&format!("[('id', '!=', {variant})]")],
        )
        .await
        .unwrap();
        seed.commit().await.unwrap();
        drop(seed);
        // the access error of a read of the product, as the debugging reader with the vendor in context
        let denied = async |orm: &Orm| {
            let mut tx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
            let reader = tx
                .env()
                .with_debug(true)
                .with_context([("partner_id", erp_sdk::Value::Int(vendor))]);
            let error = reader
                .records("product.product")
                .unwrap()
                .browse([variant])
                .unwrap()
                .cell(&mut tx, "display_name")
                .await
                .unwrap_err();
            tx.rollback().await.unwrap();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Access, "{error}");
            error.message().to_owned()
        };
        let with_supplier_read = denied(&product_policy_orm(&installed, &db).await).await;
        assert!(
            with_supplier_read.contains("[PRIVATE] Private supplier description"),
            "{with_supplier_read}"
        );
        let mut seed = installed.begin(SUPERUSER_ID).await.unwrap();
        seed.execute("DELETE FROM ir_access WHERE model_id IN (SELECT id FROM ir_model WHERE model = 'product.supplierinfo') AND operation = 'r'", &[]).await.unwrap();
        seed.commit().await.unwrap();
        drop(seed);
        let without = denied(&product_policy_orm(&installed, &db).await).await;
        assert!(
            without.contains(&format!(
                "[PUBLIC] Public chair (product.product: {variant})"
            )),
            "{without}"
        );
        assert!(
            !without.contains("PRIVATE") && !without.contains("Private supplier description"),
            "{without}"
        );
        db.cleanup().await.unwrap();
    });
}

async fn product_policy_orm(installed: &Orm, db: &TestDb) -> Orm {
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
