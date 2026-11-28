// SPDX-License-Identifier: MIT OR Apache-2.0
//! Upgrade regression for `purchase` 1.10.0 -> 1.11.0 `currency_rate` backfill when legacy
//! foreign-currency orders lack a `res.currency.rate` row on their order date.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_orm::{Context, Orm, SUPERUSER_ID};
use erp_sdk::Decimal;
use erp_testkit::TestDb;

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
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

fn build_system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons_dir());
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
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code(erp_cli::native_addons::DESCRIPTORS.iter().copied())
        .require_sql_objects(erp_cli::native_addons::SQL_MANIFESTS.iter().copied())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

async fn install_purchase(db: &TestDb) -> (ModuleSystem, Orm) {
    let system = build_system(db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let installed =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("purchase").unwrap()]))
            .await
            .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    (system, installed.orm)
}

#[test]
fn upgrade_backfills_legacy_foreign_po_without_currency_rate_row() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (system, orm) = install_purchase(&db).await;

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

        let company_id: i64 = tx
            .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let usd_id: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'USD'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let idr_id: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'IDR'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let eur_id: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'EUR'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let partner_id: i64 = tx
            .query("SELECT id::int8 FROM res_partner ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let spot_type_id: i64 = tx
            .query(
                "SELECT id::int8 FROM res_currency_rate_type WHERE code = 'spot'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);

        // Simulate a database whose Main Company switched to IDR without defining a USD rate row,
        // while EUR has a defined spot rate (0.00005 EUR per IDR -> 20,000 IDR per EUR).
        tx.execute(
            "UPDATE res_company SET currency_id = $1 WHERE id = $2",
            &[&idr_id, &company_id],
        )
        .await
        .unwrap();
        tx.execute(
            "DELETE FROM res_currency_rate WHERE currency_id = $1 AND company_id = $2",
            &[&usd_id, &company_id],
        )
        .await
        .unwrap();
        tx.execute(
            "INSERT INTO res_currency_rate (name, rate, currency_id, company_id, rate_type_id) \
             VALUES ('2026-01-01'::date, 0.00005, $1, $2, $3)",
            &[&eur_id, &company_id, &spot_type_id],
        )
        .await
        .unwrap();

        // Simulate pre-1.11.0 schema: drop currency_rate column and mark purchase at 1.10.0.
        tx.execute("ALTER TABLE purchase_order DROP COLUMN currency_rate", &[])
            .await
            .unwrap();
        tx.execute(
            "UPDATE ir_module_module SET latest_version = '1.10.0' WHERE name = 'purchase'",
            &[],
        )
        .await
        .unwrap();

        // Seed 4 legacy purchase orders covering all upgrade cases:
        // 1) Domestic IDR order
        // 2) Foreign EUR order WITH a rate row before date_order
        // 3) Historical USD order on 2026-11-17 WITHOUT any USD rate row (exact dev DB scenario)
        // 4) Historical zero-amount USD order WITHOUT any USD rate row
        let domestic_id: i64 = tx
            .query(
                "INSERT INTO purchase_order (name, partner_id, company_id, currency_id, state, \
                 date_order, amount_untaxed, amount_tax, amount_total, amount_total_cc) \
                 VALUES ('PO-DOM-1', $1, $2, $3, 'purchase', '2026-11-17 00:00:00', \
                 500000, 0, 500000, 500000) RETURNING id::int8",
                &[&partner_id, &company_id, &idr_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        let eur_po_id: i64 = tx
            .query(
                "INSERT INTO purchase_order (name, partner_id, company_id, currency_id, state, \
                 date_order, amount_untaxed, amount_tax, amount_total, amount_total_cc) \
                 VALUES ('PO-EUR-1', $1, $2, $3, 'purchase', '2026-11-17 00:00:00', \
                 10, 0, 10, 200000) RETURNING id::int8",
                &[&partner_id, &company_id, &eur_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        let usd_po_id: i64 = tx
            .query(
                "INSERT INTO purchase_order (name, partner_id, company_id, currency_id, state, \
                 date_order, amount_untaxed, amount_tax, amount_total, amount_total_cc) \
                 VALUES ('PO-USD-LEGACY', $1, $2, $3, 'purchase', '2026-11-17 00:00:00', \
                 100, 0, 100, 1600000) RETURNING id::int8",
                &[&partner_id, &company_id, &usd_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        let usd_zero_id: i64 = tx
            .query(
                "INSERT INTO purchase_order (name, partner_id, company_id, currency_id, state, \
                 date_order, amount_untaxed, amount_tax, amount_total, amount_total_cc) \
                 VALUES ('PO-USD-ZERO', $1, $2, $3, 'draft', '2026-11-17 00:00:00', \
                 0, 0, 0, 0) RETURNING id::int8",
                &[&partner_id, &company_id, &usd_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();

        // Trigger full multi-addon upgrade starting from `base` (exact production upgrade path).
        let env = orm.env(SUPERUSER_ID, Context::new());
        let upgraded =
            Box::pin(system.button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()]))
                .await
                .unwrap();
        assert!(
            upgraded.post_commit.is_clean(),
            "{:?}",
            upgraded.post_commit
        );

        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let read_rate = |rows: Vec<erp_sdk::Row>| -> Decimal { rows[0].get(0) };

        let r_dom = read_rate(
            tx.query(
                "SELECT currency_rate FROM purchase_order WHERE id = $1",
                &[&domestic_id],
            )
            .await
            .unwrap(),
        );
        let r_eur = read_rate(
            tx.query(
                "SELECT currency_rate FROM purchase_order WHERE id = $1",
                &[&eur_po_id],
            )
            .await
            .unwrap(),
        );
        let r_usd = read_rate(
            tx.query(
                "SELECT currency_rate FROM purchase_order WHERE id = $1",
                &[&usd_po_id],
            )
            .await
            .unwrap(),
        );
        let r_zero = read_rate(
            tx.query(
                "SELECT currency_rate FROM purchase_order WHERE id = $1",
                &[&usd_zero_id],
            )
            .await
            .unwrap(),
        );
        tx.commit().await.unwrap();

        assert_eq!(r_dom, Decimal::ONE);
        assert_eq!(r_eur, Decimal::new(5, 5)); // 0.00005
        assert_eq!(r_usd, Decimal::new(625, 7)); // 100 / 1,600,000 = 0.0000625
        assert_eq!(r_zero, Decimal::ONE);

        // Idempotency: a second upgrade succeeds without altering backfilled rates.
        let env2 = upgraded.orm.env(SUPERUSER_ID, Context::new());
        let second =
            Box::pin(system.button_immediate_upgrade(&env2, &[ModuleName::new("base").unwrap()]))
                .await
                .unwrap();
        assert!(second.post_commit.is_clean(), "{:?}", second.post_commit);
        let mut tx = second.orm.begin(SUPERUSER_ID).await.unwrap();
        let r_usd_after: Decimal = tx
            .query(
                "SELECT currency_rate FROM purchase_order WHERE id = $1",
                &[&usd_po_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();
        assert_eq!(r_usd_after, Decimal::new(625, 7));
    });
}

#[test]
fn upgrade_refuses_legacy_foreign_po_with_missing_rate_and_zero_company_total() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (system, orm) = install_purchase(&db).await;

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company_id: i64 = tx
            .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let usd_id: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'USD'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let idr_id: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'IDR'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let partner_id: i64 = tx
            .query("SELECT id::int8 FROM res_partner ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);

        tx.execute(
            "UPDATE res_company SET currency_id = $1 WHERE id = $2",
            &[&idr_id, &company_id],
        )
        .await
        .unwrap();
        tx.execute(
            "DELETE FROM res_currency_rate WHERE currency_id = $1 AND company_id = $2",
            &[&usd_id, &company_id],
        )
        .await
        .unwrap();
        tx.execute("ALTER TABLE purchase_order DROP COLUMN currency_rate", &[])
            .await
            .unwrap();
        tx.execute(
            "UPDATE ir_module_module SET latest_version = '1.10.0' WHERE name = 'purchase'",
            &[],
        )
        .await
        .unwrap();
        tx.execute(
            "INSERT INTO purchase_order (name, partner_id, company_id, currency_id, state, \
             date_order, amount_untaxed, amount_tax, amount_total, amount_total_cc) \
             VALUES ('PO-USD-BROKEN', $1, $2, $3, 'purchase', '2026-11-17 00:00:00', \
             250, 0, 250, 0)",
            &[&partner_id, &company_id, &usd_id],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let env = orm.env(SUPERUSER_ID, Context::new());
        let err =
            Box::pin(system.button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()]))
                .await
                .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No exchange rate is defined for currency USD"),
            "expected missing USD rate error, got: {msg}"
        );
    });
}
