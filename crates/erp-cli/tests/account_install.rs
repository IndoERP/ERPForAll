// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `account` (and so `analytic`) on a fresh database with `base` and `product`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::{Path, PathBuf};

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;

#[path = "support/install_defaults.rs"]
mod install_defaults;

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

async fn strings(tx: &mut erp_sdk::Transaction, sql: &str) -> Vec<String> {
    tx.query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect()
}

#[test]
fn account_and_analytic_install_on_base_and_product() {
    // a thread of its own: the install and the period checks overflow the test thread's stack
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(install_on_base_and_product());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn install_on_base_and_product() {
    let db = TestDb::create().await.unwrap();
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
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome = Box::pin(system.button_immediate_install(
        &env,
        &[
            ModuleName::new("account").unwrap(),
            ModuleName::new("product").unwrap(),
        ],
    ))
    .await
    .unwrap();
    assert!(outcome.post_commit.is_clean());

    let client = db.client().await.unwrap();
    for name in ["account", "analytic", "product"] {
        let rows = client
            .query(
                "SELECT state FROM ir_module_module WHERE name = $1::text",
                &[&name],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "module row {name}");
        assert_eq!(rows[0].get::<_, String>(0), "installed", "state of {name}");
    }
    // the chain model's access rows and its declared lock-wait setting. Boxed: the two halves of
    // this test in one future overflow the default test-thread stack in a debug build.
    Box::pin(async {
        let access = client
            .query(
                "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation, \
                 coalesce(a.generated, false) FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
                 LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
                 WHERE m.model = $1::text ORDER BY 1",
                &[&erp_addon_account::numbering::CHAIN_MODEL],
            )
            .await
            .unwrap();
        let permissions: Vec<String> = access
            .iter()
            .filter(|r| !r.get::<_, bool>(1))
            .map(|r| r.get(0))
            .collect();
        assert_eq!(permissions, ["base.group_user/r"], "chain permissions");
        assert_eq!(
            access.iter().filter(|r| r.get::<_, bool>(1)).count(),
            1,
            "the company restriction generated from company_access"
        );
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let wait = erp_addon_account::numbering::LockWait::from_settings(&mut tx)
            .await
            .unwrap();
        assert_eq!(
            wait.millis(),
            erp_addon_account::numbering::LOCK_WAIT_DEFAULT_MS
        );
        tx.rollback().await.unwrap();
    })
    .await;

    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();

    // reference ships no accounts, roots or code mappings (accounts come from a chart template)
    for (sql, expected) in install_defaults::ACCOUNT {
        assert_eq!(strings(&mut tx, sql).await, *expected, "{sql}");
    }
    for table in [
        "account_account",
        "account_root",
        "account_account_code",
        "account_journal",
        "account_journal_group",
    ] {
        let count = strings(&mut tx, &format!("SELECT count(*)::text FROM {table}")).await;
        assert_eq!(count, ["0"], "{table} has no default records");
    }

    // journals: read for the readonly group, everything for managers, and the generated company rule
    let journal_access = strings(
        &mut tx,
        "SELECT CASE WHEN coalesce(a.generated, false) THEN 'generated' \
         ELSE coalesce(g.module || '.' || g.name, '-') || '/' || a.operation END \
         FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
         LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
         WHERE m.model = 'account.journal' ORDER BY 1",
    )
    .await;
    assert_eq!(
        journal_access,
        [
            "account.group_account_manager/crud",
            "account.group_account_readonly/r",
            "generated"
        ]
    );

    let tag = tx.env().records("account.account.tag").unwrap();
    let master = tag
        .search(
            &mut tx,
            &erp_sdk::Domain::condition("name", erp_sdk::Operator::Eq, "Operating Activities")
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(master.len(), 1);
    let error = master.unlink(&mut tx).await.unwrap_err();
    assert!(
        error.message().contains(
            "You cannot delete this account tag (Operating Activities), it is used on the chart of account definition."
        ),
        "{:?}",
        error.message()
    );

    // no period ships (ensure_periods creates them), the readonly group reads
    // them and nobody writes them through the record API; the calendar settings read their defaults
    let count = strings(&mut tx, "SELECT count(*)::text FROM account_fiscal_period").await;
    assert_eq!(count, ["0"], "account_fiscal_period has no default records");
    let access = strings(
        &mut tx,
        "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation FROM ir_access a \
         JOIN ir_model m ON m.id = a.model_id \
         LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
         WHERE m.model = 'account.fiscal.period' AND NOT coalesce(a.generated, false) ORDER BY 1",
    )
    .await;
    assert_eq!(
        access,
        ["account.group_account_readonly/r"],
        "period permissions"
    );
    let main: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let calendar = erp_addon_account::fiscal::settings::FiscalCalendar::read(&mut tx, main)
        .await
        .unwrap();
    assert_eq!(
        (
            calendar.year_end().day(),
            calendar.year_end().month(),
            i64::from(calendar.horizon_months())
        ),
        (31, 12, erp_addon_account::fiscal::settings::HORIZON_DEFAULT)
    );
    // the fiscal year end pair is validated when written: 31 February is refused, nothing saved;
    // a fresh transaction, since the refused tag delete above poisoned this one for writes
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
    let scope = erp_sdk::views::wire::SettingsScope::Company {
        company_id: erp_sdk::views::wire::WireId::try_from(u64::try_from(main).unwrap()).unwrap(),
    };
    let env = tx.env().with_company(main).unwrap();
    let error = erp_addon_base::ir_config_setting::write_values(
        &env,
        &mut tx,
        scope,
        "0".into(),
        vec![(
            erp_addon_account::fiscal::settings::LAST_MONTH_SETTING.into(),
            erp_sdk::Value::Str("2".into()),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation, "{error:?}");
    assert!(
        error
            .message()
            .contains(erp_addon_account::fiscal::settings::LAST_DAY_SETTING),
        "{error:?}"
    );
    tx.rollback().await.unwrap();

    let today = erp_sdk::prelude::Date::today();
    let created = erp_addon_account::fiscal::ensure_periods(
        &outcome.orm,
        erp_sdk::BeginOptions::new(SUPERUSER_ID),
        main,
        today,
        today,
    )
    .await
    .unwrap();
    assert!(created >= 1, "the current period of the main company");
    let again = erp_addon_account::fiscal::ensure_periods(
        &outcome.orm,
        erp_sdk::BeginOptions::new(SUPERUSER_ID),
        main,
        today,
        today,
    )
    .await
    .unwrap();
    assert_eq!(again, 0);
    db.cleanup().await.unwrap();
}
