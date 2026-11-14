// SPDX-License-Identifier: MIT OR Apache-2.0
//! The upgrade the dev/testing deployment runs, on a database with base, web, uom, product,
//! barcodes, the GS1 nomenclature, analytic, account, stock, contacts and acceptance_notes
//! installed, after the parts this revision adds over main revision 264d3b4a96 are taken back out
//! (the release live runs is 22e2056fcb, which changed only scripts over 264d3b4a96, so the schema
//! is the same):
//! - account: the 24 `res_company` columns of the company accounting fields and the company tax
//!   defaults (with their field rows, selection rows and identities; their indexes and foreign keys
//!   go with the columns), the relation tables `product_taxes_rel`, `product_supplier_taxes_rel` and
//!   `account_account_tax_default_rel` with their field and relation rows, the company accounting
//!   form and its action `account.company_accounting_action` (which the account Settings panel links
//!   to), the product tax view and the two code mapping views; the fields this revision adds to the
//!   account form and the company tax view are cut out of their stored `arch_db`;
//! - base: the `base.partition_upkeep_interval_secs` parameter and its identity.
//!
//! A parent company with a sales and a purchase tax and a branch company without a choice of its own
//! exist before the upgrade. The chain's sequence runs three times: `load_before_operation` (no
//! `_register_hook`), then the upgrade of `base` plus the modules the chain names for this revision.
//! Run 0 starts from the rewound shape, where every effective default comes out empty because the
//! company's own choice is new too. Before run 1 the parent gets its own choice and the two stored
//! effective columns are dropped again, so the fill on existing rows has an input; runs 1 and 2 must
//! give the branch the parent's taxes. A plain load before the upgrades refuses at the Settings hook
//! (the panel link has no action); one after them passes.
//!
//! It does NOT prove: the `erp module upgrade` command itself (`live.rs` is not run here, only the
//! module-system calls it makes); an upgrade while a server holds locks on `res_company` (no
//! concurrent session runs here; the deployment chain retries a deadlock instead); an upgrade from
//! a database the previous binary built (this one is built by today's code and only partly
//! rewound: the stock Settings declarations, today's other metadata and every data row stay); the
//! effective-default fill on a table above the large-table threshold (it then runs after the
//! commit); rows other than the companies and taxes seeded here; the mail, sample_library and
//! theme_office_blue addons, which are not installed here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, SUPERUSER_ID};
use erp_sdk::{Cell, RawVals};
use erp_testkit::TestDb;
use erp_testkit::tokio_postgres::Client;
use std::path::Path;

/// The columns this revision's account adds to `res_company`.
const COMPANY_COLUMNS: [&str; 24] = [
    "transfer_account_id",
    "default_cash_difference_income_account_id",
    "default_cash_difference_expense_account_id",
    "account_journal_suspense_account_id",
    "account_journal_early_pay_discount_gain_account_id",
    "account_journal_early_pay_discount_loss_account_id",
    "income_currency_exchange_account_id",
    "expense_currency_exchange_account_id",
    "account_default_pos_receivable_account_id",
    "expense_accrual_account_id",
    "revenue_accrual_account_id",
    "currency_exchange_journal_id",
    "automatic_entry_default_journal_id",
    "bank_account_code_prefix",
    "cash_account_code_prefix",
    "transfer_account_code_prefix",
    "tax_calculation_rounding_method",
    "anglo_saxon_accounting",
    "expects_chart_of_accounts",
    "chart_template",
    "account_sale_tax_id",
    "account_purchase_tax_id",
    "effective_account_sale_tax_id",
    "effective_account_purchase_tax_id",
];
const EFFECTIVE_COLUMNS: [&str; 2] = [
    "effective_account_sale_tax_id",
    "effective_account_purchase_tax_id",
];
const RELATION_TABLES: [&str; 3] = [
    "product_taxes_rel",
    "product_supplier_taxes_rel",
    "account_account_tax_default_rel",
];
/// The view and action records this revision adds, children before parents.
const NEW_RECORDS: [&str; 5] = [
    "company_accounting_action",
    "company_accounting_form",
    "view_product_tax_choices",
    "view_account_code_mapping_list",
    "view_account_code_mapping_form",
];
/// The field elements this revision adds to views main already ships: (view, pattern).
const ADDED_ARCH: [(&str, &str); 3] = [
    ("view_account_form", r#"<field name="tax_ids"[^>]*/>"#),
    (
        "view_company_tax_settings",
        r#"<field name="account_sale_tax_id"[^>]*/>"#,
    ),
    (
        "view_company_tax_settings",
        r#"<field name="account_purchase_tax_id"[^>]*/>"#,
    ),
];
const PARAMETER_ID: &str = "partition_upkeep_interval_secs";
/// The modules the deployment chain names for this revision (changed since main and installed).
const CHAIN_MODULES: [&str; 4] = ["base", "uom", "account", "stock"];

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

async fn table_exists(client: &Client, table: &str) -> bool {
    count(
        client,
        "SELECT count(*) FROM pg_tables WHERE schemaname = current_schema() AND tablename = $1",
        &[&table],
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

/// What a fresh installation of this revision has, so the upgraded shape is compared with our code.
struct Shape {
    relation_fields: i64,
    company_foreign_keys: i64,
    company_indexes: i64,
}

async fn shape(client: &Client) -> Shape {
    Shape {
        relation_fields: count(
            client,
            "SELECT count(*) FROM ir_model_fields WHERE relation_table = ANY($1)",
            &[&RELATION_TABLES.to_vec()],
        )
        .await,
        company_foreign_keys: count(
            client,
            "SELECT count(*) FROM pg_constraint
             WHERE conrelid = 'res_company'::regclass AND contype = 'f'",
            &[],
        )
        .await,
        company_indexes: count(
            client,
            "SELECT count(*) FROM pg_indexes
             WHERE schemaname = current_schema() AND tablename = 'res_company'",
            &[],
        )
        .await,
    }
}

struct Seed {
    parent: i64,
    branch: i64,
    sale: i64,
    purchase: i64,
}

/// A parent company with a sales and a purchase tax, and a branch company below it.
async fn seed(orm: &erp_orm::Orm) -> Seed {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let parent: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = 'main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let country: i64 = tx
        .query("SELECT id::int8 FROM res_country WHERE code = 'US'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let env = tx.env().with_company(parent).unwrap();
    let companies = env.records("res.company").unwrap();
    companies
        .browse([parent])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Id(country)),
        )
        .await
        .unwrap();
    env.records("account.tax.group")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Upgrade group".into()))],
        )
        .await
        .unwrap();
    let mut taxes = Vec::new();
    for (name, role) in [("Upgrade sales", "sale"), ("Upgrade purchase", "purchase")] {
        let created = env
            .records("account.tax")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("type_tax_use", Cell::Str(role.into())),
                ],
            )
            .await
            .unwrap();
        taxes.push(created.ids()[0]);
    }
    let currency = companies
        .browse([parent])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let branch = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Upgrade branch".into()))
                    .set_cell("currency_id", currency)
                    .set_cell("parent_id", Cell::Id(parent)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    Seed {
        parent,
        branch,
        sale: taxes[0],
        purchase: taxes[1],
    }
}

/// Take views and actions out through the ORM, so their tables stay consistent.
async fn remove_records(orm: &erp_orm::Orm, module: &str, names: &[&str]) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    for name in names {
        let row = tx
            .query(
                "SELECT model, res_id FROM ir_model_data WHERE module = $1 AND name = $2",
                &[&module, name],
            )
            .await
            .unwrap();
        assert_eq!(row.len(), 1, "{module}.{name} is installed");
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

/// Drop `res_company` columns with their field rows, selection rows and identities, inside the
/// caller's transaction; `extra` adds more field rows (by id) to remove with them.
async fn drop_company_fields(client: &Client, columns: &[&str], extra: &str) {
    client
        .execute(
            &format!(
                "CREATE TEMP TABLE later_fields ON COMMIT DROP AS
                 SELECT id FROM ir_model_fields WHERE (model = 'res.company' AND name = ANY($1)){extra}"
            ),
            &[&columns.to_vec()],
        )
        .await
        .unwrap();
    let mut ddl = String::new();
    for column in columns {
        ddl.push_str(&format!("ALTER TABLE res_company DROP COLUMN {column};"));
    }
    ddl.push_str(
        "DELETE FROM ir_model_data WHERE (model = 'ir.model.fields'
                AND res_id IN (SELECT id FROM later_fields))
            OR (model = 'ir.model.fields.selection' AND res_id IN
                (SELECT id FROM ir_model_fields_selection
                 WHERE field_id IN (SELECT id FROM later_fields)));
        DELETE FROM ir_model_data WHERE model = 'ir.default' AND res_id IN
            (SELECT id FROM ir_default WHERE field_id IN (SELECT id FROM later_fields));
        DELETE FROM ir_default WHERE field_id IN (SELECT id FROM later_fields);
        DELETE FROM ir_model_fields_selection WHERE field_id IN (SELECT id FROM later_fields);
        DELETE FROM ir_model_fields WHERE id IN (SELECT id FROM later_fields);",
    );
    client.batch_execute(&ddl).await.unwrap();
}

async fn rewind_to_main(client: &Client) {
    client.batch_execute("BEGIN").await.unwrap();
    for (view, pattern) in ADDED_ARCH {
        let changed = client
            .execute(
                "UPDATE ir_ui_view SET arch_db = regexp_replace(arch_db, $2, '', 'g')
                 WHERE arch_db ~ $2 AND id =
                     (SELECT res_id FROM ir_model_data WHERE module = 'account' AND name = $1)",
                &[&view, &pattern],
            )
            .await
            .unwrap();
        assert_eq!(changed, 1, "{view} carries {pattern}");
    }
    let mut ddl = String::new();
    for table in RELATION_TABLES {
        ddl.push_str(&format!("DROP TABLE {table};"));
    }
    client.batch_execute(&ddl).await.unwrap();
    client
        .execute(
            "DELETE FROM ir_model_relation WHERE name = ANY($1)",
            &[&RELATION_TABLES.to_vec()],
        )
        .await
        .unwrap();
    drop_company_fields(
        client,
        &COMPANY_COLUMNS,
        &format!(
            " OR relation_table IN ({})",
            RELATION_TABLES.map(|t| format!("'{t}'")).join(", ")
        ),
    )
    .await;
    client
        .execute(
            "DELETE FROM ir_config_parameter WHERE key = $1",
            &[&erp_addon_base::partition_upkeep_settings::KEY],
        )
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM ir_model_data WHERE module = 'base' AND name = $1",
            &[&PARAMETER_ID],
        )
        .await
        .unwrap();
    client.batch_execute("COMMIT").await.unwrap();
}

/// Before run 1: the parent's own choice exists and the stored effective columns are new.
async fn rewind_effective(client: &Client, seed: &Seed) {
    client.batch_execute("BEGIN").await.unwrap();
    let updated = client
        .execute(
            "UPDATE res_company SET account_sale_tax_id = $1::int8,
                 account_purchase_tax_id = $2::int8
             WHERE id = $3::int8",
            &[&seed.sale, &seed.purchase, &seed.parent],
        )
        .await
        .unwrap();
    assert_eq!(updated, 1);
    drop_company_fields(client, &EFFECTIVE_COLUMNS, "").await;
    client.batch_execute("COMMIT").await.unwrap();
}

async fn effective(client: &Client, company: i64) -> (Option<i64>, Option<i64>) {
    let row = client
        .query_one(
            "SELECT effective_account_sale_tax_id::int8, effective_account_purchase_tax_id::int8
             FROM res_company WHERE id = $1::int8",
            &[&company],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

/// The settings hook refusal of a load on the rewound database.
fn assert_unavailable_link(result: Result<erp_modules::LoadOutcome, erp_modules::Error>) {
    match result {
        Ok(_) => panic!("a plain load published a Settings link whose action is missing"),
        Err(erp_modules::Error::StepFailed {
            step: erp_modules::ModuleStep::RegisterHook { model },
            source,
            ..
        }) => {
            assert_eq!(model, "ir.config.setting");
            match *source {
                erp_modules::Error::Orm(error) => assert_eq!(
                    error.message(),
                    "Setting item references an unavailable action or menu."
                ),
                other => panic!("unexpected source: {other:?}"),
            }
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

/// No `SPEC-` tag: no spec case states the deployment's module set; this is upgrade-path evidence
/// for the cycle 14 deployment (database review c14 r1, S1).
#[test]
fn dev_set_upgrades_from_the_main_shape() {
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
        let seed = seed(&installed.orm).await;
        let client = db.client().await.unwrap();
        let fresh = shape(&client).await;
        assert!(fresh.relation_fields >= 3, "the three tax relation fields");
        remove_records(&installed.orm, "account", &NEW_RECORDS).await;
        drop(installed);
        drop(init);
        drop(sys);
        rewind_to_main(&client).await;

        // the rewound shape
        for column in COMPANY_COLUMNS {
            assert!(
                !column_exists(&client, "res_company", column).await,
                "{column}"
            );
        }
        for table in RELATION_TABLES {
            assert!(!table_exists(&client, table).await, "{table}");
        }
        let rewound = shape(&client).await;
        assert_eq!(rewound.relation_fields, 0);
        assert!(rewound.company_foreign_keys < fresh.company_foreign_keys);
        assert!(rewound.company_indexes < fresh.company_indexes);
        for (view, pattern) in ADDED_ARCH {
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id
                     AND d.model = 'ir.ui.view' AND d.module = 'account' AND d.name = $1
                     WHERE v.arch_db ~ $2",
                    &[&view, &pattern],
                )
                .await,
                0,
                "{view} without {pattern}"
            );
        }

        let modules = CHAIN_MODULES.map(|name| ModuleName::new(name).unwrap());
        let sys = system(&db);
        // A server start on the rewound database: the account Settings panel links to an action
        // only the upgrade creates, so the plain load refuses.
        assert_unavailable_link(sys.load().await);
        for run in 0..3 {
            if run == 1 {
                rewind_effective(&client, &seed).await;
                for column in EFFECTIVE_COLUMNS {
                    assert!(!column_exists(&client, "res_company", column).await);
                }
            }
            // `erp module upgrade` loads the installed set first and runs the operation as the
            // caller of that load (`crates/erp-cli/src/live.rs`).
            if let Err(error) = sys.load_before_operation().await {
                panic!("load before upgrade run {run} refused: {error} ({error:?})");
            }
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
            // the Settings panel link resolves on the upgraded database
            let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
            let env = tx.env().clone();
            if let Err(error) =
                erp_addon_base::ir_config_setting::validate_declarations(&env, &mut tx).await
            {
                panic!("run {run}: Settings declarations refused: {error:?}");
            }
            tx.rollback().await.unwrap();
            drop(upgraded);

            for column in COMPANY_COLUMNS {
                assert!(
                    column_exists(&client, "res_company", column).await,
                    "run {run}: {column}"
                );
            }
            for table in RELATION_TABLES {
                assert!(table_exists(&client, table).await, "run {run}: {table}");
            }
            let now = shape(&client).await;
            assert_eq!(
                now.relation_fields, fresh.relation_fields,
                "run {run}: relation fields"
            );
            assert_eq!(
                now.company_foreign_keys, fresh.company_foreign_keys,
                "run {run}: res_company foreign keys"
            );
            assert_eq!(
                now.company_indexes, fresh.company_indexes,
                "run {run}: res_company indexes"
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_model_fields f JOIN ir_model_data d
                         ON d.model = 'ir.model.fields' AND d.res_id = f.id
                     WHERE f.model = 'res.company' AND f.name = ANY($1)",
                    &[&COMPANY_COLUMNS.to_vec()],
                )
                .await,
                i64::try_from(COMPANY_COLUMNS.len()).unwrap(),
                "run {run}: company field rows with identities"
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_model_data WHERE module = 'account' AND name = ANY($1)",
                    &[&NEW_RECORDS.to_vec()],
                )
                .await,
                i64::try_from(NEW_RECORDS.len()).unwrap(),
                "run {run}: views and action"
            );
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_model_data WHERE module = 'account'
                     AND name = 'company_accounting_action' AND model = 'ir.actions.act_window'",
                    &[],
                )
                .await,
                1,
                "run {run}: the action the Settings panel links to"
            );
            for (view, pattern) in ADDED_ARCH {
                assert_eq!(
                    count(
                        &client,
                        "SELECT count(*) FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id
                         AND d.model = 'ir.ui.view' AND d.module = 'account' AND d.name = $1
                         WHERE v.arch_db ~ $2",
                        &[&view, &pattern],
                    )
                    .await,
                    1,
                    "run {run}: {view} with {pattern}"
                );
            }
            assert_eq!(
                count(
                    &client,
                    "SELECT count(*) FROM ir_config_parameter p JOIN ir_model_data d
                         ON d.model = 'ir.config_parameter' AND d.res_id = p.id
                         AND d.module = 'base' AND d.name = $2
                     WHERE p.key = $1",
                    &[
                        &erp_addon_base::partition_upkeep_settings::KEY,
                        &PARAMETER_ID
                    ],
                )
                .await,
                1,
                "run {run}: the partition upkeep parameter and its identity"
            );
            let expected = if run == 0 {
                (None, None)
            } else {
                (Some(seed.sale), Some(seed.purchase))
            };
            assert_eq!(
                effective(&client, seed.parent).await,
                expected,
                "run {run}: parent"
            );
            assert_eq!(
                effective(&client, seed.branch).await,
                expected,
                "run {run}: the branch takes the parent's defaults"
            );
            // base stores its two-part manifest version normalized; its data is checked above
            for addon in ["uom", "product", "account", "stock"] {
                assert_eq!(
                    version(&client, addon).await,
                    manifest_version(addon),
                    "run {run}: {addon}"
                );
            }
        }
        // the server start after the upgrades
        if let Err(error) = sys.load().await {
            panic!("load after the upgrades refused: {error} ({error:?})");
        }
        drop(sys);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
