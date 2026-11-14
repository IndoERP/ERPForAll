// SPDX-License-Identifier: MIT OR Apache-2.0
//! Demo data of the `account` addon: a database created with demo data has the demo tags, the small
//! generic chart of the main company and its journals and ledger; one created without demo data has
//! none of them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::{Context, SUPERUSER_ID};
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

/// Initialize with or without demo data, install `account`, and return the queries' rows as text.
async fn install_and_read(demo: bool, queries: &[&str]) -> Vec<Vec<String>> {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(
        system.initialize(
            InitOptions::new()
                .with_demo(demo)
                .with_skip_auto_install(true),
        ),
    )
    .await
    .unwrap();
    assert!(
        init.data_notices.is_empty(),
        "initialize notices: {:?}",
        init.data_notices
    );
    let env = system.system_env(Context::new());
    let installed =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("account").unwrap()]))
            .await
            .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    // a failed demo file is rolled back and reported here, not raised
    assert!(
        installed.data_notices.is_empty(),
        "install notices: {:?}",
        installed.data_notices
    );
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let mut out = Vec::with_capacity(queries.len());
    for sql in queries {
        let rows = tx.query(sql, &[]).await.unwrap();
        out.push(rows.iter().map(|row| row.get::<_, String>(0)).collect());
    }
    tx.rollback().await.unwrap();
    drop(tx);
    drop(installed);
    db.cleanup().await.unwrap();
    out
}

/// External ids of `account` demo records, as `model:name`.
const DEMO_XIDS: &str = "SELECT d.model || ':' || d.name FROM ir_model_data d WHERE d.module = 'account' \
     AND (d.name LIKE '%\\_demo\\_%' ESCAPE '\\') ORDER BY 1";

/// Per model, how many `account` demo external ids exist.
const DEMO_COUNTS: &str = "SELECT d.model || '=' || count(*)::text FROM ir_model_data d WHERE d.module = 'account' \
     AND (d.name LIKE '%\\_demo\\_%' ESCAPE '\\') GROUP BY d.model ORDER BY d.model";

/// Each demo account: code in the main company, type, and its tags.
const ACCOUNTS: &str = "SELECT c.code || ' ' || (a.name ->> 'en_US') || ' ' || a.account_type || ' [' || \
       coalesce((SELECT string_agg(t.name, ',' ORDER BY t.name) FROM account_account_account_tag r \
                 JOIN account_account_tag t ON t.id = r.account_account_tag_id \
                 WHERE r.account_account_id = a.id), '') || ']' \
     FROM account_account a \
     JOIN ir_model_data d ON d.model = 'account.account' AND d.res_id = a.id AND d.module = 'account' \
     JOIN account_account_code c ON c.account_id = a.id \
     JOIN ir_model_data m ON m.model = 'res.company' AND m.module = 'base' AND m.name = 'main_company' \
       AND m.res_id = c.company_id \
     WHERE d.name LIKE 'account\\_demo\\_%' ESCAPE '\\' ORDER BY c.code";

/// Each demo journal: code, type, company, default account code and ledger.
const JOURNALS: &str = "SELECT j.code || ' ' || j.type || ' ' || mc.name || ' ' || coalesce(dc.code, '-') || ' ' || \
       coalesce(g.name, 'statutory') FROM account_journal j \
     JOIN ir_model_data d ON d.model = 'account.journal' AND d.res_id = j.id AND d.module = 'account' \
     JOIN ir_model_data mc ON mc.model = 'res.company' AND mc.module = 'base' AND mc.res_id = j.company_id \
     LEFT JOIN account_account_code dc ON dc.account_id = j.default_account_id AND dc.company_id = j.company_id \
     LEFT JOIN account_journal_group g ON g.id = j.journal_group_id \
     WHERE d.name LIKE 'journal\\_demo\\_%' ESCAPE '\\' ORDER BY j.sequence, j.code";

#[test]
fn account_demo_ledger_and_journals_load_with_demo() {
    big_stack(async {
        let rows = install_and_read(true, &[DEMO_COUNTS, ACCOUNTS, JOURNALS]).await;
        assert_eq!(
            rows[0],
            [
                "account.account=20",
                "account.account.tag=3",
                "account.journal=6",
                "account.journal.group=1",
            ],
            "demo records per model"
        );
        // every account type group is present, and the master cash-flow tags are used next to the demo tags
        assert_eq!(
            rows[1],
            [
                "101100 Main Bank Account asset_cash [Liquidity Review]",
                "101200 Petty Cash asset_cash [Liquidity Review]",
                "121000 Trade Receivables asset_receivable [Operating Activities]",
                "131000 Goods for Resale asset_current []",
                "141000 Prepaid Rent asset_prepayments [Premises Costs]",
                "151000 Office Equipment asset_fixed [Investing & Extraordinary Activities]",
                "211000 Trade Payables liability_payable [Operating Activities]",
                "221000 Sales Tax Payable liability_current []",
                "251000 Long-term Bank Loan liability_non_current [Financing Activities]",
                "301000 Owner Capital equity [Financing Activities]",
                "399000 Profit of the Year equity_unaffected []",
                "401000 Product Sales income []",
                "402000 Service Revenue income []",
                "481000 Interest Received income_other []",
                "501000 Cost of Goods Sold expense_direct_cost []",
                "511000 Purchased Supplies expense []",
                "611000 Office Rent expense [Premises Costs]",
                "621000 Salaries and Wages expense [Payroll Costs]",
                "681000 Depreciation of Equipment expense_depreciation []",
                "691000 Bank Charges expense_other []",
            ]
        );
        assert_eq!(
            rows[2],
            [
                "INV sale main_company 401000 statutory",
                "BILL purchase main_company 511000 statutory",
                "BNK1 bank main_company 101100 statutory",
                "CSH1 cash main_company 101200 statutory",
                "GEN general main_company - statutory",
                "MGMT general main_company - Management Reporting",
            ]
        );
    });
}

#[test]
fn account_demo_absent_without_demo() {
    big_stack(async {
        let rows = install_and_read(
            false,
            &[
                DEMO_XIDS,
                "SELECT (SELECT count(*) FROM account_account)::text || '/' || \
                 (SELECT count(*) FROM account_journal)::text || '/' || \
                 (SELECT count(*) FROM account_journal_group)::text || '/' || \
                 (SELECT count(*) FROM account_account_tag)::text",
            ],
        )
        .await;
        assert_eq!(rows[0], Vec::<String>::new(), "account demo external ids");
        // accounts / journals / ledgers / tags: only the three master tags of the default data
        assert_eq!(rows[1], ["0/0/0/3"]);
    });
}
