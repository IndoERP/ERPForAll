//! Demo data of the base and product addons: a database created with
//! demo data has the invented partners, active currencies and rates, catalog, vendor prices, pricelists,
//! combos; one created without demo data has none of them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;

use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, Orm, SUPERUSER_ID};
use erp_testkit::TestDb;

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

async fn install(db: &TestDb, demo: bool) -> Orm {
    let system = system(db);
    let init = system
        .initialize(InitOptions::new().with_demo(demo))
        .await
        .unwrap();
    assert!(
        init.data_notices.is_empty(),
        "initialize notices: {:?}",
        init.data_notices
    );
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
    assert!(
        installed.data_notices.is_empty(),
        "install notices (a failed demo file shows up here): {:?}",
        installed.data_notices
    );
    installed.orm
}

async fn strings(orm: &Orm, sql: &str) -> Vec<String> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

/// One number from a query, as text.
async fn count(orm: &Orm, sql: &str) -> String {
    strings(orm, sql).await.remove(0)
}

/// Record presence of the base demo only; not mapped to a spec case.
#[test]
fn base_demo_partners_currencies_and_rates_load_with_demo() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db, true).await;

        // six root companies (they hold a tax ID), nine child contacts
        assert_eq!(
            count(
                &orm,
                "SELECT count(*)::text FROM res_partner p JOIN ir_model_data d ON d.model='res.partner' \
                 AND d.res_id = p.id WHERE d.module='base' AND d.name LIKE 'partner_demo_%' \
                 AND p.parent_id IS NULL AND p.is_company"
            )
            .await,
            "6"
        );
        assert_eq!(
            count(
                &orm,
                "SELECT count(*)::text FROM res_partner p JOIN ir_model_data d ON d.model='res.partner' \
                 AND d.res_id = p.id WHERE d.module='base' AND d.name LIKE 'partner_demo_%' \
                 AND p.parent_id IS NOT NULL"
            )
            .await,
            "9"
        );
        assert_eq!(
            strings(
                &orm,
                "SELECT p.name || ':' || (SELECT count(*) FROM res_partner c WHERE c.parent_id = p.id)::text \
                 FROM res_partner p JOIN ir_model_data d ON d.model='res.partner' AND d.res_id = p.id \
                 WHERE d.module='base' AND d.name = 'partner_demo_cipta_digital'"
            )
            .await,
            ["Cipta Digital Mandiri:2"]
        );

        // the four extra currencies are active, the rest stay inactive
        assert_eq!(
            strings(
                &orm,
                "SELECT name FROM res_currency WHERE active AND name IN ('EUR','IDR','SGD','GBP','JPY','USD') \
                 ORDER BY name"
            )
            .await,
            ["EUR", "GBP", "IDR", "SGD", "USD"]
        );

        // seven spot rates on the main company; the newest IDR rate is the one stated for 2026-10-01
        assert_eq!(
            count(
                &orm,
                "SELECT count(*)::text FROM res_currency_rate r JOIN ir_model_data d ON d.model='res.currency.rate' \
                 AND d.res_id = r.id WHERE d.module='base' AND d.name LIKE 'rate_demo_%'"
            )
            .await,
            "7"
        );
        assert_eq!(
            strings(
                &orm,
                "SELECT t.code || ':' || c.name || ':' || r.rate::float8::text FROM res_currency_rate r \
                 JOIN res_currency c ON c.id = r.currency_id JOIN res_currency_rate_type t ON t.id = r.rate_type_id \
                 WHERE r.name = DATE '2026-10-01' AND c.name = 'IDR'"
            )
            .await,
            ["spot:IDR:16500"]
        );
    });
}

/// base demo partners; the propagation refusal itself is not asserted here)
#[test]
fn product_demo_catalog_variants_vendors_pricelists_and_combos() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db, true).await;

        assert_eq!(
            count(
                &orm,
                "SELECT count(*)::text FROM product_attribute_value v JOIN ir_model_data d \
                 ON d.model='product.attribute.value' AND d.res_id = v.id \
                 WHERE d.module='product' AND d.name LIKE 'pav_%'"
            )
            .await,
            "17"
        );
        // subcategories hang under the default categories
        assert_eq!(
            strings(
                &orm,
                "SELECT (p.name->>'en_US') || ' / ' || (c.name->>'en_US') FROM product_category c \
                 JOIN product_category p ON p.id = c.parent_id JOIN ir_model_data d \
                 ON d.model='product.category' AND d.res_id = c.id \
                 WHERE d.module='product' AND d.name LIKE 'product_category_demo_%' ORDER BY 1"
            )
            .await,
            [
                "Expenses / Travel",
                "Goods / Apparel",
                "Goods / Electronics",
                "Goods / Food and Drink",
                "Goods / Furniture",
                "Services / Consulting",
                "Services / Maintenance"
            ]
        );

        // 15 templates (12 goods, 3 services); attribute lines produce the variants
        assert_eq!(
            strings(
                &orm,
                "SELECT t.type || ':' || count(*)::text FROM product_template t JOIN ir_model_data d \
                 ON d.model='product.template' AND d.res_id = t.id \
                 WHERE d.module='product' AND d.name LIKE 'product_demo_%' GROUP BY t.type ORDER BY t.type"
            )
            .await,
            ["consu:12", "service:3"]
        );
        let variants = strings(
            &orm,
            "SELECT d.name || '=' || count(p.id)::text FROM product_template t JOIN ir_model_data d \
             ON d.model='product.template' AND d.res_id = t.id \
             JOIN product_product p ON p.product_tmpl_id = t.id \
             WHERE d.module='product' AND d.name IN ('product_demo_kaos','product_demo_meja', \
              'product_demo_kursi','product_demo_kemeja','product_demo_laptop','product_demo_rak') \
             GROUP BY d.name ORDER BY d.name",
        )
        .await;
        assert_eq!(
            variants,
            [
                "product_demo_kaos=8",
                "product_demo_kemeja=3",
                "product_demo_kursi=3",
                "product_demo_laptop=2",
                "product_demo_meja=2",
                "product_demo_rak=1"
            ]
        );
        // the value's default extra price is copied to the template's attribute value
        assert_eq!(
            strings(
                &orm,
                "SELECT (v.name->>'en_US') || ':' || tv.price_extra::float8::text \
                 FROM product_template_attribute_value tv JOIN product_attribute_value v \
                  ON v.id = tv.product_attribute_value_id JOIN ir_model_data d \
                  ON d.model='product.template' AND d.res_id = tv.product_tmpl_id \
                 WHERE d.module='product' AND d.name = 'product_demo_meja' ORDER BY v.sequence"
            )
            .await,
            ["Kayu Jati:0", "Baja:25"]
        );
        assert_eq!(
            strings(
                &orm,
                "SELECT (v.name->>'en_US') || ':' || tv.price_extra::float8::text \
                 FROM product_template_attribute_value tv JOIN product_attribute_value v \
                  ON v.id = tv.product_attribute_value_id JOIN ir_model_data d \
                  ON d.model='product.template' AND d.res_id = tv.product_tmpl_id \
                 WHERE d.module='product' AND d.name = 'product_demo_kaos' AND v.name->>'en_US' = 'XL'"
            )
            .await,
            ["XL:3"]
        );

        // vendor prices point at the base demo partners
        assert_eq!(
            count(&orm, "SELECT count(*)::text FROM product_supplierinfo").await,
            "11"
        );
        assert_eq!(
            strings(
                &orm,
                "SELECT dp.module || '.' || dp.name || ':' || count(*)::text FROM product_supplierinfo s \
                 JOIN ir_model_data dp ON dp.model='res.partner' AND dp.res_id = s.partner_id \
                 GROUP BY dp.module, dp.name ORDER BY dp.name"
            )
            .await,
            [
                "base.partner_demo_baja_sentosa:2",
                "base.partner_demo_cipta_digital:3",
                "base.partner_demo_kopi_gunung:3",
                "base.partner_demo_mitra_kayu:3"
            ]
        );

        // pricelists with their rules; combos with their items
        assert_eq!(
            strings(
                &orm,
                "SELECT (l.name->>'en_US') || ':' || c.name || ':' || \
                 (SELECT count(*) FROM product_pricelist_item i WHERE i.pricelist_id = l.id)::text \
                 FROM product_pricelist l JOIN res_currency c ON c.id = l.currency_id ORDER BY l.sequence"
            )
            .await,
            ["Reseller:USD:3", "Harga Rupiah:IDR:1"]
        );
        assert_eq!(
            strings(
                &orm,
                "SELECT (b.name->>'en_US') || ':' || count(i.id)::text FROM product_combo b \
                 JOIN product_combo_item i ON i.combo_id = b.id GROUP BY b.id, b.name ORDER BY b.id"
            )
            .await,
            [
                "Perangkat Kerja:2",
                "Perlengkapan Kantor:2",
                "Minuman Rapat:2"
            ]
        );
    });
}

/// dependency lacks demo is not asserted here)
#[test]
fn nothing_demo_exists_without_demo() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db, false).await;
        for (what, sql) in [
            (
                "demo partners",
                "SELECT count(*)::text FROM ir_model_data WHERE module='base' AND model='res.partner' AND name LIKE 'partner_demo_%'",
            ),
            ("demo rates", "SELECT count(*)::text FROM res_currency_rate"),
            (
                "product demo ids",
                "SELECT count(*)::text FROM ir_model_data WHERE module='product' AND name LIKE '%\\_demo\\_%'",
            ),
            ("templates", "SELECT count(*)::text FROM product_template"),
            (
                "suppliers",
                "SELECT count(*)::text FROM product_supplierinfo",
            ),
            ("pricelists", "SELECT count(*)::text FROM product_pricelist"),
            ("combos", "SELECT count(*)::text FROM product_combo"),
            (
                "active extra currencies",
                "SELECT count(*)::text FROM res_currency WHERE active AND name IN ('EUR','IDR','SGD','GBP')",
            ),
        ] {
            assert_eq!(count(&orm, sql).await, "0", "{what}");
        }
        // default data stays: categories, attributes, no attribute values
        assert_eq!(
            count(&orm, "SELECT count(*)::text FROM product_attribute").await,
            "5"
        );
        assert_eq!(
            count(&orm, "SELECT count(*)::text FROM product_attribute_value").await,
            "0"
        );
    });
}
