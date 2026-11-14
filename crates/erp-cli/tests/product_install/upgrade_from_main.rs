// SPDX-License-Identifier: MIT OR Apache-2.0
//! Upgrade of an installed product 1.1.0 database with rows, in the shape the 1.1.0 release left:
//! no activity columns on attributes and values, none of the later relation and integrity tables,
//! no product privilege, and a barcode mirror that counted active variants only. The reflection
//! rows (`ir_model`, `ir_model_fields`, their identities and the access rows) of the later models
//! and of the later fields listed in the fixture are deleted, and so are the unit registrations,
//! which 1.1.0 never wrote. Other metadata written by today's code stays. `base` and `product`
//! upgrade together in one run, as a deployment does.

use super::*;

/// The product version the current manifest declares, read from the addon itself.
pub(crate) fn manifest_version() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/product/manifest.toml");
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("product manifest declares a version")
        .to_owned()
}

/// One barcode claim: barcode, then its variant, template or packaging holder.
type ClaimRow = (String, Option<i64>, Option<i64>, Option<i64>);

/// Create as the fixture owner working in the main company.
async fn one(tx: &mut erp_orm::Transaction, model: &str, vals: RawVals) -> i64 {
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
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn variants_of(tx: &mut erp_orm::Transaction, template: i64) -> Vec<i64> {
    tx.query(
        "SELECT id FROM product_product WHERE product_tmpl_id=$1 ORDER BY id",
        &[&template],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

async fn relation_table(tx: &mut erp_orm::Transaction, model: &str, field: &str) -> String {
    tx.query(
        "SELECT relation_table FROM ir_model_fields WHERE model=$1 AND name=$2",
        &[&model, &field],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// with base in one run)
/// also in a read without a field list)
#[test]
fn product_upgrade_from_the_1_1_release_with_rows_and_base_in_one_run() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let (installed_system, installed) = install(&db).await;
        let manager = xid(&installed, "product", "group_product_manager").await;
        let privilege = xid(&installed, "product", "res_groups_privilege_product").await;
        let main_company = xid(&installed, "base", "main_company").await;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let name = |s: &str| RawVals::new().set_cell("name", Cell::Str(s.into()));
        let unit = one(&mut tx, "uom.uom", name("Main release unit")).await;
        let vendor = one(&mut tx, "res.partner", name("Main release vendor")).await;
        let category = one(&mut tx, "product.category", name("Main release category")).await;
        let single = one(
            &mut tx,
            "product.template",
            name("Main single")
                .set_cell("categ_id", Cell::Id(category))
                .set_cell("barcode", Cell::Str("MAIN-SINGLE".into())),
        )
        .await;
        let archived_family = one(&mut tx, "product.template", name("Main archived sibling")).await;
        let active_variant = variants_of(&mut tx, archived_family).await[0];
        let archived_variant = one(
            &mut tx,
            "product.product",
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(archived_family))
                .set_cell("active", Cell::Bool(false))
                .set_cell("barcode", Cell::Str("MAIN-ARCHIVED".into())),
        )
        .await;
        let attribute = one(
            &mut tx,
            "product.attribute",
            name("Main size").set_cell("create_variant", Cell::Str("always".into())),
        )
        .await;
        let mut values = Vec::new();
        for label in ["Main S", "Main M"] {
            values.push(
                one(
                    &mut tx,
                    "product.attribute.value",
                    name(label).set_cell("attribute_id", Cell::Id(attribute)),
                )
                .await,
            );
        }
        let configured = one(&mut tx, "product.template", name("Main configured")).await;
        one(
            &mut tx,
            "product.template.attribute.line",
            RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(configured))
                .set_cell("attribute_id", Cell::Id(attribute))
                .set_commands(
                    "value_ids",
                    [Command::set(
                        values.iter().map(|id| RecId::from_real(*id).unwrap()),
                    )],
                ),
        )
        .await;
        let configured_variants = variants_of(&mut tx, configured).await;
        assert_eq!(configured_variants.len(), 2);
        tx.env()
            .records("product.product")
            .unwrap()
            .browse([configured_variants[0]])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("barcode", Cell::Str("MAIN-VARIANT".into())),
            )
            .await
            .unwrap();
        let supplier = one(
            &mut tx,
            "product.supplierinfo",
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("product_tmpl_id", Cell::Id(single))
                .set_cell("company_id", Cell::Id(main_company))
                .set_cell("price", Cell::Decimal(erp_sdk::Decimal::new(1250, 2)))
                .set_cell("uom_id", Cell::Id(unit)),
        )
        .await;
        let single_variant = variants_of(&mut tx, single).await[0];
        let packaging = one(
            &mut tx,
            "product.uom",
            RawVals::new()
                .set_cell("product_id", Cell::Id(single_variant))
                .set_cell("uom_id", Cell::Id(unit))
                .set_cell("barcode", Cell::Str("MAIN-PACK".into())),
        )
        .await;
        let pricelist = one(&mut tx, "product.pricelist", name("Main pricelist")).await;
        one(
            &mut tx,
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("fixed_price", Cell::Decimal(erp_sdk::Decimal::ONE)),
        )
        .await;
        // The 1.1.0 release had no product privilege and no identity for it.
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([manager])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("privilege_id", Cell::Null))
            .await
            .unwrap();
        tx.env()
            .records("res.groups.privilege")
            .unwrap()
            .browse([privilege])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let template_units = relation_table(&mut tx, "product.template", "uom_ids").await;
        let variant_units = relation_table(&mut tx, "product.product", "extra_uom_ids").await;
        // Schema-owner fixture: the 1.1.0 mirror followed the only active variant.
        tx.execute(
            "UPDATE product_product SET barcode='MAIN-MIRROR' WHERE id=$1",
            &[&active_variant],
        )
        .await
        .unwrap();
        tx.execute(
            "UPDATE product_template SET barcode='MAIN-MIRROR' WHERE id=$1",
            &[&archived_family],
        )
        .await
        .unwrap();
        tx.batch(&format!(
            "ALTER TABLE product_attribute DROP COLUMN active;
            ALTER TABLE product_attribute_value DROP COLUMN active, DROP COLUMN is_custom,
                DROP COLUMN html_color;
            DROP TABLE \"{template_units}\", \"{variant_units}\",
                product_attribute_custom_value, product_barcode_claim, product_barcode_scope;
            UPDATE ir_module_module SET latest_version='1.1.0' WHERE name='product';
            CREATE TEMP TABLE later_models AS SELECT id FROM ir_model
                WHERE model IN ('product.attribute.custom.value', 'product.barcode.claim',
                    'product.barcode.scope');
            CREATE TEMP TABLE later_fields AS SELECT f.id FROM ir_model_fields f
                WHERE f.model_id IN (SELECT id FROM later_models)
                OR (f.model, f.name) IN (('product.attribute', 'active'),
                    ('product.attribute', 'product_tmpl_ids'),
                    ('product.attribute.value', 'active'), ('product.attribute.value', 'is_custom'),
                    ('product.attribute.value', 'html_color'), ('product.template', 'uom_ids'),
                    ('product.product', 'extra_uom_ids'));
            DELETE FROM ir_model_data d WHERE (d.model = 'ir.model'
                    AND d.res_id IN (SELECT id FROM later_models))
                OR (d.model = 'ir.model.fields' AND d.res_id IN (SELECT id FROM later_fields))
                OR (d.model = 'ir.access' AND d.res_id IN (SELECT a.id FROM ir_access a
                    WHERE a.model_id IN (SELECT id FROM later_models)));
            DELETE FROM ir_access WHERE model_id IN (SELECT id FROM later_models);
            DELETE FROM ir_model_fields WHERE id IN (SELECT id FROM later_fields);
            DELETE FROM ir_model WHERE id IN (SELECT id FROM later_models);
            DROP TABLE later_models, later_fields;
            ALTER TABLE uom_reference_use DISABLE TRIGGER erp_uom_marker_immutable;
            DELETE FROM uom_reference_use;
            ALTER TABLE uom_reference_use ENABLE TRIGGER erp_uom_marker_immutable;"
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        drop(installed_system);
        drop(installed);

        let system = system(&db);
        let modules = [
            ModuleName::new("base").unwrap(),
            ModuleName::new("product").unwrap(),
        ];
        for run in 0..2 {
            let upgraded = match system
                .button_immediate_upgrade(&system.system_env(Context::new()), &modules)
                .await
            {
                Ok(upgraded) => upgraded,
                Err(error) => panic!("upgrade run {run} from 1.1.0 refused: {error} ({error:?})"),
            };
            assert!(
                upgraded.post_commit.is_clean(),
                "{:?}",
                upgraded.post_commit
            );
            let orm = &upgraded.orm;
            assert_eq!(
                strings(
                    orm,
                    "SELECT latest_version FROM ir_module_module WHERE name='product'"
                )
                .await,
                [manifest_version()]
            );
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let env = tx
                .env()
                .with_context([("active_test", erp_sdk::Value::Bool(false))]);
            // attributes and values keep their rows and become visible
            for (model, id) in [
                ("product.attribute", attribute),
                ("product.attribute.value", values[0]),
                ("product.attribute.value", values[1]),
            ] {
                assert_eq!(
                    env.records(model)
                        .unwrap()
                        .browse([id])
                        .unwrap()
                        .cell(&mut tx, "active")
                        .await
                        .unwrap(),
                    Cell::Bool(true),
                    "{model} {id}"
                );
            }
            // the attribute in use lists its template, also in a read without a field list
            let attribute_row = env
                .records("product.attribute")
                .unwrap()
                .browse([attribute])
                .unwrap();
            let all = attribute_row.read(&mut tx, &[]).await.unwrap();
            assert_eq!(
                all[0]["product_tmpl_ids"],
                attribute_row
                    .read(&mut tx, &["product_tmpl_ids"])
                    .await
                    .unwrap()[0]["product_tmpl_ids"]
            );
            assert_eq!(
                attribute_row
                    .cell(&mut tx, "product_tmpl_ids")
                    .await
                    .unwrap(),
                Cell::Ids(vec![RecId::from_real(configured).unwrap()].into())
            );
            // the later models and fields are reflected again with their identities, and the
            // 1.3 backfill registered the unit the product rows use
            let reflected: i64 = tx
                .query(
                    "SELECT (SELECT count(*) FROM ir_model m JOIN ir_model_data d
                            ON d.model = 'ir.model' AND d.res_id = m.id AND d.module = 'product'
                        WHERE m.model IN ('product.attribute.custom.value',
                            'product.barcode.claim', 'product.barcode.scope'))
                        + (SELECT count(*) FROM ir_model_fields
                        WHERE (model, name) IN (('product.attribute', 'active'),
                            ('product.attribute', 'product_tmpl_ids'),
                            ('product.attribute.value', 'is_custom'),
                            ('product.template', 'uom_ids'), ('product.product', 'extra_uom_ids')))",
                    &[],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(reflected, 8);
            let registered: i64 = tx
                .query(
                    "SELECT count(*) FROM uom_reference_use WHERE uom_id = $1",
                    &[&unit],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(registered, 1, "unit {unit} registered by the upgrade");
            // the legacy mirror moved to its variant; nothing else changed hands
            let template = env.records("product.template").unwrap();
            let variant = env.records("product.product").unwrap();
            assert_eq!(
                template
                    .browse([archived_family])
                    .unwrap()
                    .cell(&mut tx, "barcode")
                    .await
                    .unwrap(),
                Cell::Null
            );
            for (id, barcode, active) in [
                (active_variant, "MAIN-MIRROR", true),
                (archived_variant, "MAIN-ARCHIVED", false),
                (configured_variants[0], "MAIN-VARIANT", true),
            ] {
                let row = variant.browse([id]).unwrap();
                assert_eq!(
                    row.cell(&mut tx, "barcode").await.unwrap(),
                    Cell::Str(barcode.into())
                );
                assert_eq!(
                    row.cell(&mut tx, "active").await.unwrap(),
                    Cell::Bool(active)
                );
            }
            assert_eq!(
                template
                    .browse([single])
                    .unwrap()
                    .cell(&mut tx, "barcode")
                    .await
                    .unwrap(),
                Cell::Str("MAIN-SINGLE".into())
            );
            let claims = tx
                .query(
                    "SELECT barcode, variant_id, template_id, packaging_id
                    FROM product_barcode_claim ORDER BY barcode",
                    &[],
                )
                .await
                .unwrap();
            let claims: Vec<ClaimRow> = claims
                .iter()
                .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
                .collect();
            assert_eq!(
                claims,
                vec![
                    (
                        "MAIN-ARCHIVED".to_owned(),
                        Some(archived_variant),
                        None,
                        None
                    ),
                    ("MAIN-MIRROR".to_owned(), Some(active_variant), None, None),
                    ("MAIN-PACK".to_owned(), None, None, Some(packaging)),
                    ("MAIN-SINGLE".to_owned(), Some(single_variant), None, None),
                    (
                        "MAIN-VARIANT".to_owned(),
                        Some(configured_variants[0]),
                        None,
                        None
                    ),
                ]
            );
            // vendor prices keep their terms
            let seller = env
                .records("product.supplierinfo")
                .unwrap()
                .browse([supplier])
                .unwrap();
            assert_eq!(
                seller.cell(&mut tx, "price").await.unwrap(),
                Cell::Decimal(erp_sdk::Decimal::new(1250, 2))
            );
            assert_eq!(
                seller.cell(&mut tx, "uom_id").await.unwrap(),
                Cell::Id(unit)
            );
            // the manager group gets the new privilege
            let new_privilege: i64 = tx
                .query(
                    "SELECT res_id FROM ir_model_data WHERE module='product'
                    AND name='res_groups_privilege_product'",
                    &[],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(
                env.records("res.groups")
                    .unwrap()
                    .browse([manager])
                    .unwrap()
                    .cell(&mut tx, "privilege_id")
                    .await
                    .unwrap(),
                Cell::Id(new_privilege)
            );
            tx.rollback().await.unwrap();
        }
        drop(system);
        db.cleanup().await.unwrap();
    });
}
