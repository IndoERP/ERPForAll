// SPDX-License-Identifier: MIT OR Apache-2.0
//! The upgrade the dev/testing deployment runs (`base,acceptance_notes`, which marks every installed
//! native addon for upgrade), on a database with base, web, uom, product, barcodes, the GS1
//! nomenclature, analytic, account, stock, contacts and acceptance_notes installed, after
//! the parts this revision added over main revision 1c93ac1c64 are taken back out:
//! - account: the tax, tax group and repartition-line tables, the tag relation table, their
//!   reflection rows, identities and access rows, the tax views, actions and menus, and the five
//!   company columns of the tax and lock slices with their field rows;
//! - stock: the columns it adds to `res_company`, `res_partner` and `res_users` with their field
//!   rows and the two partner location defaults (`ir.default` rows and identities) it seeds;
//!   version 1.0.0, so its 1.1 `pre` step is selected and those base tables wait for stock's
//!   step while account and barcodes load (database review r6, S2);
//! - product: version 1.1.0 only;
//! - base: the `base.attachment_max_bytes` parameter and its identity;
//! - the main company's barcode nomenclature is cleared, so the barcodes data function writes a
//!   company while those tables wait.
//!
//! It does NOT prove: an upgrade from a database the previous binary built (this one is built by
//! today's code and only partly rewound; today's metadata elsewhere stays); the stock 1.0.0 shape
//! of its own tables and SQL objects (`stock_install/upgrade_from_main.rs`) or the product 1.1.0
//! shape (`product_install/upgrade_from_main.rs`); rows other than default data; the
//! mail, sample_library and theme_office_blue, which are not installed here (with mail, installing
//! product auto-installs product_mail, which the dev/testing database does not have).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;
use erp_testkit::tokio_postgres::Client;
use std::path::Path;

const TAX_MODELS: [&str; 3] = [
    "account.tax",
    "account.tax.group",
    "account.tax.repartition.line",
];
/// Children before parents: menus, the inheriting company view, the views, then the actions.
const TAX_RECORDS: [&str; 11] = [
    "menu_taxes",
    "menu_tax_groups",
    "view_company_tax_settings",
    "view_tax_group_list",
    "view_tax_group_form",
    "view_tax_list",
    "view_tax_form",
    "view_tax_repartition_list",
    "view_tax_repartition_form",
    "action_taxes",
    "action_tax_groups",
];
const ACCOUNT_COMPANY_COLUMNS: [&str; 5] = [
    "account_fiscal_country_id",
    "account_price_include",
    "tax_lock_date",
    "sale_lock_date",
    "purchase_lock_date",
];
const STOCK_COLUMNS: [(&str, &str); 4] = [
    ("res_company", "internal_transit_location_id"),
    ("res_partner", "property_stock_customer"),
    ("res_partner", "property_stock_supplier"),
    ("res_users", "property_warehouse_id"),
];
const STOCK_FIELDS: [(&str, &str); 6] = [
    ("res.company", "internal_transit_location_id"),
    ("res.company", "scrap_location_id"),
    ("res.company", "default_stock_location_id"),
    ("res.partner", "property_stock_customer"),
    ("res.partner", "property_stock_supplier"),
    ("res.users", "property_warehouse_id"),
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
            &erp_addon_acceptance_notes::ADDON,
            &erp_addon_barcodes::ADDON,
            &erp_addon_barcodes_gs1_nomenclature::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_stock::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
        ])
        .build()
        .unwrap()
}

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

fn manifest_version(addon: &str) -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../addons/{addon}/manifest.toml"));
    let manifest: toml::Table = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    manifest["version"].as_str().unwrap().to_owned()
}

async fn count(
    client: &Client,
    sql: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn column_exists(client: &Client, table: &str, column: &str) -> bool {
    count(
        client,
        "SELECT count(*) FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        &[&table, &column],
    )
    .await
        == 1
}

async fn version(client: &Client, addon: &str) -> String {
    client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name = $1",
            &[&addon],
        )
        .await
        .unwrap()
        .get(0)
}

/// Take the tax views, actions and menus out through the ORM, so their tables stay consistent.
async fn remove_tax_records(orm: &erp_orm::Orm) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    for name in TAX_RECORDS {
        let row = tx
            .query(
                "SELECT model, res_id FROM ir_model_data WHERE module = 'account' AND name = $1",
                &[&name],
            )
            .await
            .unwrap();
        assert_eq!(row.len(), 1, "account.{name} is installed");
        let model: String = row[0].get(0);
        let id: i64 = row[0].get(1);
        tx.env()
            .records(&model)
            .unwrap()
            .browse([id])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

async fn rewind_to_main(client: &Client) {
    let tax_records: Vec<&str> = TAX_RECORDS.to_vec();
    let tax_models: Vec<&str> = TAX_MODELS.to_vec();
    let account_columns: Vec<&str> = ACCOUNT_COMPANY_COLUMNS.to_vec();
    client.batch_execute("BEGIN").await.unwrap();
    client
        .execute(
            "DELETE FROM ir_model_data WHERE module = 'account' AND name = ANY($1)",
            &[&tax_records],
        )
        .await
        .unwrap();
    client
        .execute(
            "CREATE TEMP TABLE later_models ON COMMIT DROP AS
             SELECT id, model FROM ir_model WHERE model = ANY($1)",
            &[&tax_models],
        )
        .await
        .unwrap();
    assert_eq!(
        count(client, "SELECT count(*) FROM later_models", &[]).await,
        3
    );
    client
        .execute(
            "CREATE TEMP TABLE later_fields ON COMMIT DROP AS
             SELECT id FROM ir_model_fields WHERE model_id IN (SELECT id FROM later_models)
             OR (model = 'res.company' AND name = ANY($1))",
            &[&account_columns],
        )
        .await
        .unwrap();
    for (model, name) in STOCK_FIELDS {
        client
            .execute(
                "INSERT INTO later_fields SELECT id FROM ir_model_fields WHERE model = $1 AND name = $2",
                &[&model, &name],
            )
            .await
            .unwrap();
    }
    let relations: Vec<String> = client
        .query(
            "SELECT DISTINCT relation_table FROM ir_model_fields
             WHERE ttype = 'many2many' AND relation_table IS NOT NULL
             AND model_id IN (SELECT id FROM later_models)",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        !relations.is_empty(),
        "the repartition line has a tag relation"
    );
    let mut ddl = String::new();
    for table in &relations {
        ddl.push_str(&format!("DROP TABLE \"{table}\";"));
    }
    ddl.push_str("DROP TABLE account_tax_repartition_line, account_tax, account_tax_group;");
    for column in ACCOUNT_COMPANY_COLUMNS {
        ddl.push_str(&format!("ALTER TABLE res_company DROP COLUMN {column};"));
    }
    for (table, column) in STOCK_COLUMNS {
        ddl.push_str(&format!("ALTER TABLE {table} DROP COLUMN {column};"));
    }
    client.batch_execute(&ddl).await.unwrap();
    client
        .execute(
            "DELETE FROM ir_model_relation WHERE name = ANY($1)",
            &[&relations],
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "DELETE FROM ir_model_data d WHERE (d.model = 'ir.model'
                    AND d.res_id IN (SELECT id FROM later_models))
                OR (d.model = 'ir.model.fields' AND d.res_id IN (SELECT id FROM later_fields))
                OR (d.model = 'ir.access' AND d.res_id IN (SELECT a.id FROM ir_access a
                    WHERE a.model_id IN (SELECT id FROM later_models)));
            DELETE FROM ir_access WHERE model_id IN (SELECT id FROM later_models);
            DELETE FROM ir_model_data WHERE model = 'ir.default' AND res_id IN
                (SELECT id FROM ir_default WHERE field_id IN (SELECT id FROM later_fields));
            DELETE FROM ir_default WHERE field_id IN (SELECT id FROM later_fields);
            DELETE FROM ir_model_fields_selection WHERE field_id IN (SELECT id FROM later_fields);
            DELETE FROM ir_model_fields WHERE id IN (SELECT id FROM later_fields);
            DELETE FROM ir_model WHERE id IN (SELECT id FROM later_models);
            DELETE FROM ir_config_parameter WHERE key = 'base.attachment_max_bytes';
            DELETE FROM ir_model_data WHERE module = 'base' AND name = 'attachment_max_bytes';
            UPDATE res_company SET nomenclature_id = NULL WHERE id =
                (SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_company');
            UPDATE ir_module_module SET latest_version = '1.0.0' WHERE name = 'stock';
            UPDATE ir_module_module SET latest_version = '1.1.0' WHERE name = 'product';
            COMMIT;",
        )
        .await
        .unwrap();
}

/// No `SPEC-` tag: no spec case states the deployment's module set; this is upgrade-path evidence
/// for the cycle 13 deployment (database review r6, S1 and S2).
#[test]
fn dev_set_upgrades_twice_from_the_main_shape() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let sys = system(&db);
        let init = Box::pin(sys.initialize(InitOptions::new().with_skip_auto_install(true)))
            .await
            .unwrap();
        assert!(init.post_commit.is_clean());
        let names = ["stock", "account", "contacts", "acceptance_notes"]
            .map(|name| ModuleName::new(name).unwrap());
        let installed = Box::pin(
            sys.button_immediate_install(&init.orm.env(SUPERUSER_ID, Context::new()), &names),
        )
        .await
        .unwrap();
        assert!(
            installed.post_commit.is_clean(),
            "{:?}",
            installed.post_commit
        );
        remove_tax_records(&installed.orm).await;
        drop(installed);
        drop(init);
        drop(sys);
        let client = db.client().await.unwrap();
        rewind_to_main(&client).await;

        // the rewound shape
        for table in [
            "account_tax",
            "account_tax_group",
            "account_tax_repartition_line",
        ] {
            assert_eq!(
                count(&client, "SELECT count(*) FROM pg_tables WHERE schemaname = current_schema() AND tablename = $1", &[&table]).await,
                0,
                "{table}"
            );
        }
        for column in ACCOUNT_COMPANY_COLUMNS {
            assert!(
                !column_exists(&client, "res_company", column).await,
                "{column}"
            );
        }
        for (table, column) in STOCK_COLUMNS {
            assert!(
                !column_exists(&client, table, column).await,
                "{table}.{column}"
            );
        }
        assert_eq!(version(&client, "stock").await, "1.0.0");
        assert_eq!(version(&client, "product").await, "1.1.0");
        assert_eq!(
            version(&client, "account").await,
            manifest_version("account")
        );
        assert_eq!(
            count(
                &client,
                "SELECT count(*) FROM res_company WHERE nomenclature_id IS NULL",
                &[]
            )
            .await,
            1
        );

        let access_csv = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../addons/account/security/ir.access.csv"),
        )
        .unwrap();
        let tax_access = i64::try_from(
            access_csv
                .lines()
                .filter(|line| line.contains(",model_account_tax"))
                .count(),
        )
        .unwrap();
        assert!(tax_access > 0);

        let modules = ["base", "acceptance_notes"].map(|name| ModuleName::new(name).unwrap());
        let sys = system(&db);
        for run in 0..2 {
            let upgraded = match Box::pin(
                sys.button_immediate_upgrade(&sys.system_env(Context::new()), &modules),
            )
            .await
            {
                Ok(upgraded) => upgraded,
                Err(error) => panic!("upgrade run {run} refused: {error} ({error:?})"),
            };
            assert!(
                upgraded.post_commit.is_clean(),
                "{:?}",
                upgraded.post_commit
            );
            drop(upgraded);

            for table in [
                "account_tax",
                "account_tax_group",
                "account_tax_repartition_line",
            ] {
                assert_eq!(
                    count(&client, "SELECT count(*) FROM pg_tables WHERE schemaname = current_schema() AND tablename = $1", &[&table]).await,
                    1,
                    "run {run}: {table}"
                );
            }
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_access a JOIN ir_model m ON m.id = a.model_id
                     JOIN ir_model_data d ON d.model = 'ir.access' AND d.res_id = a.id
                         AND d.module = 'account'
                     WHERE m.model = ANY($1)",
                    &[&TAX_MODELS.to_vec()],
                )
                .await,
                tax_access,
                "run {run}: tax access rows"
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM information_schema.columns
                     WHERE table_schema = current_schema() AND table_name = 'res_company'
                     AND column_name = 'account_price_include' AND is_nullable = 'NO'",
                    &[],
                )
                .await,
                1,
                "run {run}: account_price_include is required"
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM res_company WHERE account_price_include IS NULL",
                    &[]
                )
                .await,
                0
            );
            for column in ACCOUNT_COMPANY_COLUMNS {
                assert!(
                    column_exists(&client, "res_company", column).await,
                    "run {run}: {column}"
                );
            }
            for (table, column) in STOCK_COLUMNS {
                assert!(
                    column_exists(&client, table, column).await,
                    "run {run}: {table}.{column}"
                );
            }
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM res_company WHERE nomenclature_id IS NULL",
                    &[]
                )
                .await,
                0,
                "run {run}: every company has its barcode nomenclature"
            );
            let tax_records: Vec<&str> = TAX_RECORDS.to_vec();
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_model_data WHERE module = 'account' AND name = ANY($1)",
                    &[&tax_records],
                )
                .await,
                i64::try_from(TAX_RECORDS.len()).unwrap(),
                "run {run}: tax views, actions and menus"
            );
            assert_eq!(
                count(&client, "SELECT count(*) FROM ir_config_parameter WHERE key = 'base.attachment_max_bytes'", &[]).await,
                1
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_model_data d JOIN ir_default v ON v.id = d.res_id
                     WHERE d.module = 'stock' AND d.model = 'ir.default'",
                    &[],
                )
                .await,
                2,
                "run {run}: stock seeds both partner location defaults once"
            );
            for addon in ["stock", "product", "account"] {
                assert_eq!(
                    version(&client, addon).await,
                    manifest_version(addon),
                    "run {run}: {addon}"
                );
            }
        }
        assert_eq!(version(&client, "stock").await, "1.4.0");
        assert_eq!(version(&client, "product").await, "1.5.0");
        drop(sys);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
