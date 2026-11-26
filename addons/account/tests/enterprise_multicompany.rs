// SPDX-License-Identifier: MIT OR Apache-2.0
//! Multi-company integration tests for ERPForAll's 4 enterprise financial pillars:
//! 1. Feature toggles in Accounting Settings (SME/UMKM default-off light groups)
//! 2. Multi-company Cost Center Controlling, Locks, Validity & SKF Allocations
//! 3. Multi-company Asset CIP Accumulation, Multi-Receiver Settlement & Reversal
//! 4. Multi-company Advanced Budgeting, Real-Time AVC, Workbench Amendments & Commitments
//! 5. Multi-company Multi-Currency Group Financial Consolidation (3-rate FX, CTA, IC Elim, NCI)
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests"
)]

use erp_addon_account::budget::avc::{
    CommitmentRequest, check_and_record_commitments, release_commitments_for_source,
};
use erp_sdk::{Command, Context, Decimal, ErrorKind, prelude::*};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
    .before_schema(
        "CREATE TABLE IF NOT EXISTS ir_model_data (\
         id bigserial PRIMARY KEY, \
         module text NOT NULL, \
         name text NOT NULL, \
         model text NOT NULL, \
         res_id int8 NOT NULL, \
         noupdate boolean NOT NULL DEFAULT false\
         )",
    )
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap()
}

async fn seed_security_groups(tx: &mut Transaction) {
    for (module, name, label) in [
        ("base", "group_user", "Internal User"),
        ("base", "group_system", "Settings"),
        ("base", "group_erp_manager", "Access Rights"),
        ("account", "group_account_readonly", "Accounting Readonly"),
        ("account", "group_account_user", "Accounting User"),
        ("account", "group_account_manager", "Accounting Manager"),
        ("account", "group_budget_override", "Budget Override"),
        (
            "account",
            "group_cost_center_controlling",
            "Cost Center Controlling",
        ),
        ("account", "group_asset_cip", "Asset & CIP"),
        ("account", "group_budget_avc", "Budget AVC"),
        (
            "account",
            "group_group_consolidation",
            "Group Consolidation",
        ),
    ] {
        let gid: i64 = tx
            .query(
                "INSERT INTO res_groups (name) VALUES (jsonb_build_object('en_US', $1::text)) RETURNING id::int8",
                &[&label],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.execute(
            "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
             VALUES ($1, $2, 'res.groups', $3, true)",
            &[&module, &name, &gid],
        )
        .await
        .unwrap();
    }
}

async fn ensure_currency(tx: &mut Transaction, code: &str, symbol: &str) -> i64 {
    if let Some(row) = tx
        .query(
            "SELECT id::int8 FROM res_currency WHERE name = $1 LIMIT 1",
            &[&code],
        )
        .await
        .unwrap()
        .first()
    {
        return row.get(0);
    }
    tx.env()
        .records("res.currency")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(code.into()))
                    .set_cell("symbol", Cell::Str(symbol.into()))
                    .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2)))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn ensure_rate_type(tx: &mut Transaction, code: &str, name: &str) -> i64 {
    if let Some(row) = tx
        .query(
            "SELECT id::int8 FROM res_currency_rate_type WHERE code = $1 LIMIT 1",
            &[&code],
        )
        .await
        .unwrap()
        .first()
    {
        return row.get(0);
    }
    tx.env()
        .records("res.currency.rate.type")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("code", Cell::Str(code.into()))
                    .set_cell("name", Cell::Str(name.into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn make_company(tx: &mut Transaction, name: &str, currency_id: i64) -> i64 {
    tx.env()
        .records("res.company")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("currency_id", Cell::Id(currency_id)),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

fn co_env(tx: &Transaction, company_id: i64) -> Env {
    tx.env().with_company(company_id).unwrap()
}

async fn make_account(
    tx: &mut Transaction,
    company_id: i64,
    name: &str,
    code: &str,
    account_type: &str,
) -> i64 {
    co_env(tx, company_id)
        .records("account.account")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("code", Cell::Str(code.into()))
                    .set_cell("account_type", Cell::Str(account_type.into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

/// A journal written directly: the journal rules read declared settings that a harness without
/// data files does not install (mirrors `tests/invoices.rs`).
async fn make_journal(tx: &mut Transaction, company_id: i64, name: &str, code: &str) -> i64 {
    tx.query(
        "INSERT INTO account_journal (name, code, active, type, company_id, sequence, \
         refund_sequence, payment_sequence, sequence_reset, sequence_padding, color, show_on_dashboard) \
         VALUES (jsonb_build_object('en_US', $2::text), $3::text, true, 'general', $1, 10, \
         false, false, 'month', 4, 0, true) RETURNING id::int8",
        &[&company_id, &name, &code],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn make_plan(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records("account.analytic.plan")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()[0]
}

fn dec(s: &str) -> Decimal {
    s.parse::<Decimal>().unwrap()
}

fn d(y: i32, m: u32, day: u32) -> Date {
    Date::from_ymd(y, m, day).unwrap()
}

#[tokio::test]
async fn feature_toggles_are_light_groups_declared_in_accounting_settings() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // 1. All 4 enterprise feature groups are registered as light groups in ResGroups.
    let light_groups: Vec<String> = tx
        .env()
        .records("res.groups")
        .unwrap()
        .call(&mut tx, "_get_light_group_xmlids", ())
        .await
        .unwrap();
    for expected in [
        erp_addon_account::COST_CENTER_GROUP,
        erp_addon_account::ASSET_CIP_GROUP,
        erp_addon_account::BUDGET_AVC_GROUP,
        erp_addon_account::CONSOLIDATION_GROUP,
    ] {
        assert!(
            light_groups.iter().any(|g| g == expected),
            "Expected light group {expected} in {light_groups:?}"
        );
    }

    // 2. All 4 feature toggles exist in the Accounting Settings panel declarations as group toggles.
    let items = erp_addon_account::company::settings::items().unwrap();
    for (key, expected_group) in [
        (
            "account.group_cost_center_controlling",
            erp_addon_account::COST_CENTER_GROUP,
        ),
        (
            "account.group_asset_cip",
            erp_addon_account::ASSET_CIP_GROUP,
        ),
        (
            "account.group_budget_avc",
            erp_addon_account::BUDGET_AVC_GROUP,
        ),
        (
            "account.group_group_consolidation",
            erp_addon_account::CONSOLIDATION_GROUP,
        ),
    ] {
        let item = items
            .iter()
            .find(|it| it.key() == key)
            .unwrap_or_else(|| panic!("Expected Accounting Settings group_toggle '{key}'"));
        match item.kind() {
            erp_sdk::settings::PanelItemKind::Group { group, .. } => {
                assert_eq!(group.as_str(), expected_group);
            }
            other => panic!("Expected PanelItemKind::Group for '{key}', got {other:?}"),
        }
    }

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multicompany_cost_center_isolation_locks_and_skf_allocation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_security_groups(&mut tx).await;

    let usd = ensure_currency(&mut tx, "USD", "$").await;
    let co_a = make_company(&mut tx, "Alpha Manufacturing Co.", usd).await;
    let co_b = make_company(&mut tx, "Beta Logistics Co.", usd).await;
    let plan = make_plan(&mut tx, "Cost Centers").await;

    let j_a = make_journal(&mut tx, co_a, "General A", "GENA").await;
    let cash_a = make_account(&mut tx, co_a, "Cash A", "1010", "asset_cash").await;
    let exp_a = make_account(&mut tx, co_a, "IT Expense A", "6100", "expense").await;
    let rev_a = make_account(&mut tx, co_a, "Sales Revenue A", "4100", "income").await;

    // Create Cost Centers in Company A and Company B
    let cc_it_a = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("IT Shared Services".into()))
                    .set_cell("code", Cell::Str("CC-IT-A".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a))
                    .set_cell("center_type", Cell::Str("cost_center".into()))
                    .set_cell("lock_revenues", Cell::Bool(true))
                    .set_cell("valid_from", Cell::Date(d(2026, 1, 1)))
                    .set_cell("valid_to", Cell::Date(d(2026, 12, 31))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cc_prod_a = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Plant A".into()))
                    .set_cell("code", Cell::Str("CC-PROD-A".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cc_sales_a = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Sales A".into()))
                    .set_cell("code", Cell::Str("CC-SALES-A".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cc_b = co_env(&tx, co_b)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Beta Depot".into()))
                    .set_cell("code", Cell::Str("CC-BETA".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_b)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    // 1. Cross-company Cost Center assignment must be refused.
    let cross_err = tx
        .savepoint(async |tx| {
            let mv = co_env(tx, co_a)
                .records("account.move")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("move_type", Cell::Str("entry".into()))
                            .set_cell("journal_id", Cell::Id(j_a))
                            .set_cell("date", Cell::Date(d(2026, 6, 15)))
                            .set_commands(
                                "line_ids",
                                vec![
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(exp_a))
                                            .set_cell("name", Cell::Str("Cross-co test".into()))
                                            .set_cell("debit", Cell::Decimal(dec("100.00")))
                                            .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("analytic_account_id", Cell::Id(cc_b)),
                                    ),
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(cash_a))
                                            .set_cell("name", Cell::Str("Cash".into()))
                                            .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("credit", Cell::Decimal(dec("100.00"))),
                                    ),
                                ],
                            ),
                    ],
                )
                .await?;
            mv.call::<(), ()>(tx, "action_post", ()).await
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            cross_err.kind(),
            ErrorKind::Validation | ErrorKind::UserError
        ),
        "Expected cross-company refusal, got: {cross_err}"
    );

    // 2. Direct revenue posting to lock_revenues Cost Center must be blocked.
    let lock_err = tx
        .savepoint(async |tx| {
            let mv = co_env(tx, co_a)
                .records("account.move")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("move_type", Cell::Str("entry".into()))
                            .set_cell("journal_id", Cell::Id(j_a))
                            .set_cell("date", Cell::Date(d(2026, 6, 15)))
                            .set_commands(
                                "line_ids",
                                vec![
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(cash_a))
                                            .set_cell("name", Cell::Str("Cash".into()))
                                            .set_cell("debit", Cell::Decimal(dec("500.00")))
                                            .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                                    ),
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(rev_a))
                                            .set_cell("name", Cell::Str("Bad Direct Rev".into()))
                                            .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("credit", Cell::Decimal(dec("500.00")))
                                            .set_cell("analytic_account_id", Cell::Id(cc_it_a)),
                                    ),
                                ],
                            ),
                    ],
                )
                .await?;
            mv.call::<(), ()>(tx, "action_post", ()).await
        })
        .await
        .unwrap_err();
    assert!(
        lock_err.message().contains("locked for direct revenue"),
        "Unexpected lock error: {}",
        lock_err.message()
    );

    // 3. Validity window enforcement: posting in 2027 must fail for CC-IT-A (valid 2026 only).
    let valid_err = tx
        .savepoint(async |tx| {
            let mv = co_env(tx, co_a)
                .records("account.move")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("move_type", Cell::Str("entry".into()))
                            .set_cell("journal_id", Cell::Id(j_a))
                            .set_cell("date", Cell::Date(d(2027, 1, 10)))
                            .set_commands(
                                "line_ids",
                                vec![
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(exp_a))
                                            .set_cell("name", Cell::Str("Expired CC".into()))
                                            .set_cell("debit", Cell::Decimal(dec("100.00")))
                                            .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("analytic_account_id", Cell::Id(cc_it_a)),
                                    ),
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(cash_a))
                                            .set_cell("name", Cell::Str("Cash".into()))
                                            .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("credit", Cell::Decimal(dec("100.00"))),
                                    ),
                                ],
                            ),
                    ],
                )
                .await?;
            mv.call::<(), ()>(tx, "action_post", ()).await
        })
        .await
        .unwrap_err();
    assert!(
        valid_err.message().contains("valid"),
        "Unexpected validity error: {}",
        valid_err.message()
    );

    // 4. Post $2,000 primary IT expense -> SKF 60/40 allocation -> verify exact $1,200/$800 -> Reverse.
    let it_mv = co_env(&tx, co_a)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("date", Cell::Date(d(2026, 6, 15)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(exp_a))
                                    .set_cell("name", Cell::Str("Cloud Infrastructure".into()))
                                    .set_cell("debit", Cell::Decimal(dec("2000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("analytic_account_id", Cell::Id(cc_it_a)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(cash_a))
                                    .set_cell("name", Cell::Str("Cash".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("2000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    it_mv
        .call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    let skf = co_env(&tx, co_a)
        .records("account.analytic.skf")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Headcount".into()))
                    .set_cell("code", Cell::Str("SKF-HC".into()))
                    .set_cell("company_id", Cell::Id(co_a))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("analytic_account_id", Cell::Id(cc_prod_a))
                                    .set_cell("date", Cell::Date(d(2026, 6, 1)))
                                    .set_cell("quantity", Cell::Decimal(dec("60"))),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("analytic_account_id", Cell::Id(cc_sales_a))
                                    .set_cell("date", Cell::Date(d(2026, 6, 1)))
                                    .set_cell("quantity", Cell::Decimal(dec("40"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let cycle = co_env(&tx, co_a)
        .records("account.cost.allocation.cycle")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("IT Overhead".into()))
                    .set_cell("code", Cell::Str("CYC-IT".into()))
                    .set_cell("allocation_method", Cell::Str("assessment".into()))
                    .set_cell("tracing_basis", Cell::Str("skf".into()))
                    .set_cell("skf_id", Cell::Id(skf))
                    .set_cell("company_id", Cell::Id(co_a))
                    .set_commands(
                        "rule_ids",
                        vec![Command::create(
                            RawVals::new()
                                .set_cell("name", Cell::Str("IT -> Ops".into()))
                                .set_cell("sender_center_id", Cell::Id(cc_it_a))
                                .set_cell("sender_share_pct", Cell::Decimal(dec("100")))
                                .set_commands(
                                    "receiver_ids",
                                    vec![
                                        Command::create(
                                            RawVals::new()
                                                .set_cell("receiver_center_id", Cell::Id(cc_prod_a))
                                                .set_cell("fixed_pct", Cell::Decimal(dec("60"))),
                                        ),
                                        Command::create(
                                            RawVals::new()
                                                .set_cell(
                                                    "receiver_center_id",
                                                    Cell::Id(cc_sales_a),
                                                )
                                                .set_cell("fixed_pct", Cell::Decimal(dec("40"))),
                                        ),
                                    ],
                                ),
                        )],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let run = co_env(&tx, co_a)
        .records("account.cost.allocation.run")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("June 2026 Allocation".into()))
                    .set_cell("cycle_id", Cell::Id(cycle))
                    .set_cell("date_from", Cell::Date(d(2026, 6, 1)))
                    .set_cell("date_to", Cell::Date(d(2026, 6, 30)))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap();
    run.call::<(), ()>(&mut tx, "action_execute_allocation", ())
        .await
        .unwrap();

    let bal_it: Decimal = tx
        .query(
            "SELECT COALESCE(SUM(amount), 0) FROM account_analytic_line WHERE account_id = $1",
            &[&cc_it_a],
        )
        .await
        .unwrap()[0]
        .get(0);
    let bal_prod: Decimal = tx
        .query(
            "SELECT COALESCE(SUM(amount), 0) FROM account_analytic_line WHERE account_id = $1",
            &[&cc_prod_a],
        )
        .await
        .unwrap()[0]
        .get(0);
    let bal_sales: Decimal = tx
        .query(
            "SELECT COALESCE(SUM(amount), 0) FROM account_analytic_line WHERE account_id = $1",
            &[&cc_sales_a],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(bal_it, Decimal::ZERO);
    assert_eq!(bal_prod, dec("-1200.00"));
    assert_eq!(bal_sales, dec("-800.00"));

    // Reverse allocation -> sender back to -2000, receivers back to 0.
    run.call::<(), ()>(&mut tx, "action_reverse_allocation", ())
        .await
        .unwrap();
    let bal_it_rev: Decimal = tx
        .query(
            "SELECT COALESCE(SUM(amount), 0) FROM account_analytic_line WHERE account_id = $1",
            &[&cc_it_a],
        )
        .await
        .unwrap()[0]
        .get(0);
    let bal_prod_rev: Decimal = tx
        .query(
            "SELECT COALESCE(SUM(amount), 0) FROM account_analytic_line WHERE account_id = $1",
            &[&cc_prod_a],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(bal_it_rev, dec("-2000.00"));
    assert_eq!(bal_prod_rev, Decimal::ZERO);

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multicompany_asset_cip_accumulation_settlement_and_reversal() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_security_groups(&mut tx).await;

    let usd = ensure_currency(&mut tx, "USD", "$").await;
    let co_a = make_company(&mut tx, "CapEx Holdings", usd).await;
    let co_b = make_company(&mut tx, "Other Entity", usd).await;
    let plan = make_plan(&mut tx, "Departments").await;

    let j_a = make_journal(&mut tx, co_a, "General A", "GENA").await;
    let j_b = make_journal(&mut tx, co_b, "General B", "GENB").await;
    let cash_a = make_account(&mut tx, co_a, "Bank A", "1010", "asset_cash").await;
    let cip_acc = make_account(&mut tx, co_a, "CIP Clearing", "1590", "asset_non_current").await;
    let fa_acc = make_account(&mut tx, co_a, "Fixed Assets", "1510", "asset_fixed").await;
    let accum_dep = make_account(&mut tx, co_a, "Accum Dep", "1519", "asset_non_current").await;
    let dep_exp = make_account(&mut tx, co_a, "Dep Expense", "6810", "expense_depreciation").await;
    let op_exp = make_account(&mut tx, co_a, "Site Cleanup", "6100", "expense").await;
    let cash_b = make_account(&mut tx, co_b, "Bank B", "1010", "asset_cash").await;
    let exp_b = make_account(&mut tx, co_b, "Exp B", "6100", "expense").await;

    let cc_prod = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Plant A".into()))
                    .set_cell("code", Cell::Str("CC-PLANT".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let cls_cip = co_env(&tx, co_a)
        .records("account.asset.category")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("CIP Projects".into()))
                    .set_cell("code", Cell::Str("CLS-CIP".into()))
                    .set_cell("asset_type", Cell::Str("cip".into()))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("account_asset_id", Cell::Id(cip_acc))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cls_bldg = co_env(&tx, co_a)
        .records("account.asset.category")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Buildings".into()))
                    .set_cell("code", Cell::Str("CLS-BLDG".into()))
                    .set_cell("asset_type", Cell::Str("normal".into()))
                    .set_cell("method", Cell::Str("linear".into()))
                    .set_cell("method_number", Cell::Int(20))
                    .set_cell("method_period", Cell::Int(12))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("account_asset_id", Cell::Id(fa_acc))
                    .set_cell("account_depreciation_id", Cell::Id(accum_dep))
                    .set_cell("account_depreciation_expense_id", Cell::Id(dep_exp))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cls_mach = co_env(&tx, co_a)
        .records("account.asset.category")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Machinery".into()))
                    .set_cell("code", Cell::Str("CLS-MACH".into()))
                    .set_cell("asset_type", Cell::Str("normal".into()))
                    .set_cell("method", Cell::Str("linear".into()))
                    .set_cell("method_number", Cell::Int(60))
                    .set_cell("method_period", Cell::Int(1))
                    .set_cell("prorata_temporis", Cell::Bool(true))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("account_asset_id", Cell::Id(fa_acc))
                    .set_cell("account_depreciation_id", Cell::Id(accum_dep))
                    .set_cell("account_depreciation_expense_id", Cell::Id(dep_exp))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let cip = co_env(&tx, co_a)
        .records("account.asset")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Factory Wing B (CIP)".into()))
                    .set_cell("code", Cell::Str("CIP-001".into()))
                    .set_cell("asset_type", Cell::Str("cip".into()))
                    .set_cell("category_id", Cell::Id(cls_cip))
                    .set_cell("analytic_account_id", Cell::Id(cc_prod))
                    .set_cell("acquisition_date", Cell::Date(d(2026, 2, 1)))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("account_asset_id", Cell::Id(cip_acc))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap();
    let cip_id = cip.ids()[0];
    cip.call::<(), ()>(&mut tx, "action_validate", ())
        .await
        .unwrap();

    // 1. Zero-depreciation invariant on CIP asset.
    let dep_err = tx
        .savepoint(async |tx| {
            cip.call::<(), ()>(tx, "action_compute_depreciation_board", ())
                .await
        })
        .await
        .unwrap_err();
    assert!(
        dep_err.message().contains("Construction in Progress"),
        "Expected CIP depreciation refusal, got: {}",
        dep_err.message()
    );

    // 2. Cross-company CIP assignment on Company B move must be refused.
    let cross_cip_err = tx
        .savepoint(async |tx| {
            co_env(tx, co_b)
                .records("account.move")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("move_type", Cell::Str("entry".into()))
                            .set_cell("journal_id", Cell::Id(j_b))
                            .set_cell("date", Cell::Date(d(2026, 3, 1)))
                            .set_commands(
                                "line_ids",
                                vec![
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(exp_b))
                                            .set_cell("name", Cell::Str("Wrong Co CIP".into()))
                                            .set_cell("debit", Cell::Decimal(dec("100.00")))
                                            .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("asset_id", Cell::Id(cip_id)),
                                    ),
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("account_id", Cell::Id(cash_b))
                                            .set_cell("name", Cell::Str("Bank B".into()))
                                            .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                            .set_cell("credit", Cell::Decimal(dec("100.00"))),
                                    ),
                                ],
                            ),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            cross_cip_err.kind(),
            ErrorKind::Validation | ErrorKind::UserError
        ),
        "Expected cross-company CIP refusal, got: {cross_cip_err}"
    );

    // 3. Accumulate $30,000 onto CIP via posted move.
    let cap_mv = co_env(&tx, co_a)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("date", Cell::Date(d(2026, 3, 10)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(cip_acc))
                                    .set_cell("name", Cell::Str("Turnkey Construction".into()))
                                    .set_cell("debit", Cell::Decimal(dec("30000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("asset_id", Cell::Id(cip_id)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(cash_a))
                                    .set_cell("name", Cell::Str("Bank A".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("30000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    cap_mv
        .call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    assert_eq!(
        cip.cell(&mut tx, "cip_accumulated_amount").await.unwrap(),
        Cell::Decimal(dec("30000.00"))
    );

    // 4. Multi-receiver Settlement (70% New Building, 20% New Machine, 10% Cost Center Expense).
    for vals in [
        RawVals::new()
            .set_cell("asset_id", Cell::Id(cip_id))
            .set_cell("sequence", Cell::Int(10))
            .set_cell("name", Cell::Str("Wing B Structure".into()))
            .set_cell("receiver_type", Cell::Str("new_asset".into()))
            .set_cell("percentage", Cell::Decimal(dec("70")))
            .set_cell("target_category_id", Cell::Id(cls_bldg))
            .set_cell("analytic_account_id", Cell::Id(cc_prod))
            .set_cell("auto_confirm_asset", Cell::Bool(true)),
        RawVals::new()
            .set_cell("asset_id", Cell::Id(cip_id))
            .set_cell("sequence", Cell::Int(20))
            .set_cell("name", Cell::Str("Conveyor Line".into()))
            .set_cell("receiver_type", Cell::Str("new_asset".into()))
            .set_cell("percentage", Cell::Decimal(dec("20")))
            .set_cell("target_category_id", Cell::Id(cls_mach))
            .set_cell("analytic_account_id", Cell::Id(cc_prod))
            .set_cell("auto_confirm_asset", Cell::Bool(true)),
        RawVals::new()
            .set_cell("asset_id", Cell::Id(cip_id))
            .set_cell("sequence", Cell::Int(30))
            .set_cell("name", Cell::Str("Cleanup Expense".into()))
            .set_cell("receiver_type", Cell::Str("cost_center".into()))
            .set_cell("percentage", Cell::Decimal(dec("10")))
            .set_cell("expense_account_id", Cell::Id(op_exp))
            .set_cell("analytic_account_id", Cell::Id(cc_prod)),
    ] {
        co_env(&tx, co_a)
            .records("account.asset.settlement.rule")
            .unwrap()
            .create(&mut tx, vec![vals])
            .await
            .unwrap();
    }

    let settle = co_env(&tx, co_a)
        .records("account.asset.settlement")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Final Capitalization".into()))
                    .set_cell("cip_asset_id", Cell::Id(cip_id))
                    .set_cell("settlement_date", Cell::Date(d(2026, 4, 1)))
                    .set_cell("settlement_mode", Cell::Str("full".into())),
            ],
        )
        .await
        .unwrap();
    settle
        .call::<(), ()>(&mut tx, "action_post_settlement", ())
        .await
        .unwrap();

    assert_eq!(
        cip.cell(&mut tx, "state").await.unwrap(),
        Cell::Str("settled".into())
    );
    assert_eq!(
        cip.cell(&mut tx, "cip_unsettled_amount").await.unwrap(),
        Cell::Decimal(Decimal::ZERO)
    );
    let spawned_count: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_asset WHERE source_cip_id = $1",
            &[&cip_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(spawned_count, 2);

    // 5. Reverse settlement -> CIP restored to cip (open accumulation) with $30,000 unsettled.
    settle
        .call::<(), ()>(&mut tx, "action_reverse_settlement", ())
        .await
        .unwrap();
    assert_eq!(
        cip.cell(&mut tx, "state").await.unwrap(),
        Cell::Str("cip".into())
    );
    assert_eq!(
        cip.cell(&mut tx, "cip_unsettled_amount").await.unwrap(),
        Cell::Decimal(dec("30000.00"))
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multicompany_budget_avc_hard_stop_workbench_and_live_post_refresh() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_security_groups(&mut tx).await;

    let usd = ensure_currency(&mut tx, "USD", "$").await;
    let co_a = make_company(&mut tx, "Budget Co A", usd).await;
    let co_b = make_company(&mut tx, "Budget Co B", usd).await;
    let plan = make_plan(&mut tx, "Departments").await;

    let j_a = make_journal(&mut tx, co_a, "General A", "GENA").await;
    let cash_a = make_account(&mut tx, co_a, "Cash A", "1010", "asset_cash").await;
    let exp_a = make_account(&mut tx, co_a, "Maintenance A", "6100", "expense").await;

    let cc_a1 = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Plant A".into()))
                    .set_cell("code", Cell::Str("CC-A1".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cc_a2 = co_env(&tx, co_a)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Sales A".into()))
                    .set_cell("code", Cell::Str("CC-A2".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let cc_b1 = co_env(&tx, co_b)
        .records("account.analytic.account")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Plant B".into()))
                    .set_cell("code", Cell::Str("CC-B1".into()))
                    .set_cell("plan_id", Cell::Id(plan))
                    .set_cell("company_id", Cell::Id(co_b)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let prof_a = co_env(&tx, co_a)
        .records("account.budget.profile")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Strict Hard Stop".into()))
                    .set_cell("code", Cell::Str("AVC-STRICT".into()))
                    .set_cell("avc_mode", Cell::Str("hard_stop".into()))
                    .set_cell("warning_threshold_pct", Cell::Decimal(dec("80")))
                    .set_cell("blocking_threshold_pct", Cell::Decimal(dec("100")))
                    .set_cell("tolerance_amount", Cell::Decimal(Decimal::ZERO))
                    .set_cell("check_commitments", Cell::Bool(true))
                    .set_cell("company_id", Cell::Id(co_a)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    // Company A budget: $10,000 on CC-A1, $5,000 on CC-A2.
    let bud_a = co_env(&tx, co_a)
        .records("account.budget")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("FY2026 Budget A".into()))
                    .set_cell("budget_type", Cell::Str("opex".into()))
                    .set_cell("profile_id", Cell::Id(prof_a))
                    .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                    .set_cell("date_to", Cell::Date(d(2026, 12, 31)))
                    .set_cell("company_id", Cell::Id(co_a))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("name", Cell::Str("Plant A OPEX".into()))
                                    .set_cell("analytic_account_id", Cell::Id(cc_a1))
                                    .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                                    .set_cell("date_to", Cell::Date(d(2026, 12, 31)))
                                    .set_cell("original_amount", Cell::Decimal(dec("10000.00")))
                                    .set_cell("planned_amount", Cell::Decimal(dec("10000.00")))
                                    .set_cell("release_pct", Cell::Decimal(dec("100")))
                                    .set_cell("released_amount", Cell::Decimal(dec("10000.00"))),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("name", Cell::Str("Sales A OPEX".into()))
                                    .set_cell("analytic_account_id", Cell::Id(cc_a2))
                                    .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                                    .set_cell("date_to", Cell::Date(d(2026, 12, 31)))
                                    .set_cell("original_amount", Cell::Decimal(dec("5000.00")))
                                    .set_cell("planned_amount", Cell::Decimal(dec("5000.00")))
                                    .set_cell("release_pct", Cell::Decimal(dec("100")))
                                    .set_cell("released_amount", Cell::Decimal(dec("5000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    bud_a
        .call::<(), ()>(&mut tx, "action_budget_confirm", ())
        .await
        .unwrap();
    bud_a
        .call::<(), ()>(&mut tx, "action_budget_validate", ())
        .await
        .unwrap();

    // Company B has a large $90,000 budget — must never bleed into Company A's AVC!
    let bud_b = co_env(&tx, co_b)
        .records("account.budget")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("FY2026 Budget B".into()))
                    .set_cell("budget_type", Cell::Str("opex".into()))
                    .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                    .set_cell("date_to", Cell::Date(d(2026, 12, 31)))
                    .set_cell("company_id", Cell::Id(co_b))
                    .set_commands(
                        "line_ids",
                        vec![Command::create(
                            RawVals::new()
                                .set_cell("name", Cell::Str("Plant B OPEX".into()))
                                .set_cell("analytic_account_id", Cell::Id(cc_b1))
                                .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                                .set_cell("date_to", Cell::Date(d(2026, 12, 31)))
                                .set_cell("original_amount", Cell::Decimal(dec("90000.00")))
                                .set_cell("planned_amount", Cell::Decimal(dec("90000.00")))
                                .set_cell("release_pct", Cell::Decimal(dec("100")))
                                .set_cell("released_amount", Cell::Decimal(dec("90000.00"))),
                        )],
                    ),
            ],
        )
        .await
        .unwrap();
    bud_b
        .call::<(), ()>(&mut tx, "action_budget_confirm", ())
        .await
        .unwrap();
    bud_b
        .call::<(), ()>(&mut tx, "action_budget_validate", ())
        .await
        .unwrap();

    let bl_a1_id: i64 = tx
        .query(
            "SELECT id::int8 FROM account_budget_line WHERE budget_id = $1 AND analytic_account_id = $2",
            &[&bud_a.ids()[0], &cc_a1],
        )
        .await
        .unwrap()[0]
        .get(0);
    let bl_a2_id: i64 = tx
        .query(
            "SELECT id::int8 FROM account_budget_line WHERE budget_id = $1 AND analytic_account_id = $2",
            &[&bud_a.ids()[0], &cc_a2],
        )
        .await
        .unwrap()[0]
        .get(0);

    // 1. AVC Hard-Stop blocks $12,000 PO commitment against $10,000 released budget on Company A.
    let env_a = co_env(&tx, co_a);
    let block_err = tx
        .savepoint(async |tx| {
            check_and_record_commitments(
                tx,
                &env_a,
                &[CommitmentRequest {
                    company_id: co_a,
                    source_model: "purchase.order.line".to_owned(),
                    source_id: 9001,
                    source_ref: "PO-OVER-001".to_owned(),
                    account_id: Some(exp_a),
                    analytic_account_id: Some(cc_a1),
                    asset_id: None,
                    date: d(2026, 6, 1),
                    amount: dec("12000.00"),
                }],
            )
            .await
        })
        .await
        .unwrap_err();
    assert!(
        block_err
            .message()
            .contains("Budget Availability Control exceeded"),
        "Unexpected AVC error: {}",
        block_err.message()
    );

    // 2. Zero-sum Transfer (Virement): unbalanced transfer refused; balanced +2000/-2000 approved!
    let bad_virement = tx
        .savepoint(async |tx| {
            let amd = co_env(tx, co_a)
                .records("account.budget.amendment")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Unbalanced Transfer".into()))
                            .set_cell("budget_id", Cell::Id(bud_a.ids()[0]))
                            .set_cell("amendment_type", Cell::Str("transfer".into()))
                            .set_cell("date", Cell::Date(d(2026, 6, 1)))
                            .set_commands(
                                "line_ids",
                                vec![
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("budget_line_id", Cell::Id(bl_a1_id))
                                            .set_cell("amount", Cell::Decimal(dec("1000.00"))),
                                    ),
                                    Command::create(
                                        RawVals::new()
                                            .set_cell("budget_line_id", Cell::Id(bl_a2_id))
                                            .set_cell("amount", Cell::Decimal(dec("-500.00"))),
                                    ),
                                ],
                            ),
                    ],
                )
                .await?;
            amd.call::<(), ()>(tx, "action_approve", ()).await
        })
        .await
        .unwrap_err();
    assert!(
        bad_virement.message().contains("balance to 0.00"),
        "Expected zero-sum transfer error, got: {}",
        bad_virement.message()
    );

    let good_virement = co_env(&tx, co_a)
        .records("account.budget.amendment")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Reallocate Sales -> Plant".into()))
                    .set_cell("budget_id", Cell::Id(bud_a.ids()[0]))
                    .set_cell("amendment_type", Cell::Str("transfer".into()))
                    .set_cell("date", Cell::Date(d(2026, 6, 1)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("budget_line_id", Cell::Id(bl_a1_id))
                                    .set_cell("amount", Cell::Decimal(dec("3000.00"))),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("budget_line_id", Cell::Id(bl_a2_id))
                                    .set_cell("amount", Cell::Decimal(dec("-3000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    good_virement
        .call::<(), ()>(&mut tx, "action_approve", ())
        .await
        .unwrap();

    // CC-A1 released is now $13,000 -> book $8,000 PO commitment + post $4,000 live expense!
    check_and_record_commitments(
        &mut tx,
        &env_a,
        &[CommitmentRequest {
            company_id: co_a,
            source_model: "purchase.order.line".to_owned(),
            source_id: 9002,
            source_ref: "PO-OK-002".to_owned(),
            account_id: Some(exp_a),
            analytic_account_id: Some(cc_a1),
            asset_id: None,
            date: d(2026, 6, 2),
            amount: dec("8000.00"),
        }],
    )
    .await
    .unwrap();

    let exp_mv = co_env(&tx, co_a)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_a))
                    .set_cell("date", Cell::Date(d(2026, 6, 3)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(exp_a))
                                    .set_cell("name", Cell::Str("Live Maintenance Spend".into()))
                                    .set_cell("debit", Cell::Decimal(dec("4000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("analytic_account_id", Cell::Id(cc_a1)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(cash_a))
                                    .set_cell("name", Cell::Str("Cash".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("4000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    exp_mv
        .call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    // Immediately after action_post (without manual refresh!), practical_amount == 4000, committed == 8000!
    let bl_row = tx
        .query(
            "SELECT released_amount, committed_amount, practical_amount FROM account_budget_line WHERE id = $1",
            &[&bl_a1_id],
        )
        .await
        .unwrap();
    let released: Decimal = bl_row[0].get(0);
    let committed: Decimal = bl_row[0].get(1);
    let practical: Decimal = bl_row[0].get(2);
    assert_eq!(released, dec("13000.00"));
    assert_eq!(committed, dec("8000.00"));
    assert_eq!(practical, dec("4000.00"));

    // Release PO commitment -> committed drops cleanly to 0.00.
    release_commitments_for_source(&mut tx, "purchase.order.line", &[9002])
        .await
        .unwrap();
    let comm_after: Decimal = tx
        .query(
            "SELECT committed_amount FROM account_budget_line WHERE id = $1",
            &[&bl_a1_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(comm_after, Decimal::ZERO);

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multicompany_multicurrency_consolidation_cta_ic_elimination_and_nci() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_security_groups(&mut tx).await;

    let usd = ensure_currency(&mut tx, "USD", "$").await;
    let idr = ensure_currency(&mut tx, "IDR", "Rp").await;
    let rt_close = ensure_rate_type(&mut tx, "closing", "Closing").await;
    let rt_avg = ensure_rate_type(&mut tx, "monthly_average", "Monthly Average").await;
    let rt_hist = ensure_rate_type(&mut tx, "historical", "Historical").await;
    let _rt_spot = ensure_rate_type(&mut tx, "spot", "Spot").await;

    let parent_co = make_company(&mut tx, "IndoERP Holding USD", usd).await;
    let sub_co = make_company(&mut tx, "PT Nusantara Subsidiary IDR", idr).await;

    // Tag Trading Partners on both companies' partners.
    let parent_partner: i64 = tx
        .query(
            "SELECT partner_id::int8 FROM res_company WHERE id = $1",
            &[&parent_co],
        )
        .await
        .unwrap()[0]
        .get(0);
    let sub_partner: i64 = tx
        .query(
            "SELECT partner_id::int8 FROM res_company WHERE id = $1",
            &[&sub_co],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "UPDATE res_partner SET trading_partner_company_id = $2 WHERE id = $1",
        &[&parent_partner, &parent_co],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE res_partner SET trading_partner_company_id = $2 WHERE id = $1",
        &[&sub_partner, &sub_co],
    )
    .await
    .unwrap();

    // Seed 3-rate FX on parent company: Closing 15,000, Average 14,500, Historical 14,000 IDR/USD.
    for (rt_id, rate) in [(rt_close, "15000"), (rt_avg, "14500"), (rt_hist, "14000")] {
        co_env(&tx, parent_co)
            .records("res.currency.rate")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("currency_id", Cell::Id(idr))
                        .set_cell("company_id", Cell::Id(parent_co))
                        .set_cell("rate_type_id", Cell::Id(rt_id))
                        .set_cell("name", Cell::Date(d(2026, 1, 1)))
                        .set_cell("rate", Cell::Decimal(dec(rate))),
                ],
            )
            .await
            .unwrap();
    }

    // Parent USD chart & journal
    let j_p = make_journal(&mut tx, parent_co, "Parent General", "GENP").await;
    let p_cash = make_account(&mut tx, parent_co, "Parent Cash", "1010", "asset_cash").await;
    let p_ar = make_account(&mut tx, parent_co, "Parent AR", "1210", "asset_receivable").await;
    let p_cap = make_account(&mut tx, parent_co, "Parent Capital", "3010", "equity").await;
    let p_rev = make_account(&mut tx, parent_co, "Parent Revenue", "4010", "income").await;

    // Subsidiary IDR chart & journal
    let j_s = make_journal(&mut tx, sub_co, "Sub General", "GENS").await;
    let s_cash = make_account(&mut tx, sub_co, "Sub Cash IDR", "1101", "asset_cash").await;
    let s_ap = make_account(&mut tx, sub_co, "Sub AP IDR", "2101", "liability_payable").await;
    let s_cap = make_account(&mut tx, sub_co, "Sub Capital IDR", "3101", "equity").await;
    let s_rev = make_account(&mut tx, sub_co, "Sub Revenue IDR", "4101", "income").await;
    let s_exp = make_account(&mut tx, sub_co, "Sub Expense IDR", "5101", "expense").await;

    // Parent Capital: Dr Cash $50,000 / Cr Share Capital $50,000
    let pm1 = co_env(&tx, parent_co)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_p))
                    .set_cell("date", Cell::Date(d(2026, 1, 10)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(p_cash))
                                    .set_cell("name", Cell::Str("Parent Paid-in Cash".into()))
                                    .set_cell("debit", Cell::Decimal(dec("50000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(p_cap))
                                    .set_cell("name", Cell::Str("Parent Capital".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("50000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    pm1.call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    // Subsidiary Capital: Dr Cash 140,000,000 IDR / Cr Capital 140,000,000 IDR
    let sm1 = co_env(&tx, sub_co)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_s))
                    .set_cell("date", Cell::Date(d(2026, 1, 15)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_cash))
                                    .set_cell("name", Cell::Str("Sub Paid-in Cash".into()))
                                    .set_cell("debit", Cell::Decimal(dec("140000000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_cap))
                                    .set_cell("name", Cell::Str("Sub Capital".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("140000000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    sm1.call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    // Subsidiary External Sales: 72,500,000 IDR (= $5,000 at 14,500 avg)
    let sm2 = co_env(&tx, sub_co)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_s))
                    .set_cell("date", Cell::Date(d(2026, 6, 15)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_cash))
                                    .set_cell("name", Cell::Str("External Sales Cash".into()))
                                    .set_cell("debit", Cell::Decimal(dec("72500000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_rev))
                                    .set_cell("name", Cell::Str("External Sales".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("72500000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    sm2.call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    // Intercompany Management Fee: Parent bills $2,000, Sub books 30,000,000 IDR
    let ic_p = co_env(&tx, parent_co)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_p))
                    .set_cell("date", Cell::Date(d(2026, 6, 20)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(p_ar))
                                    .set_cell("partner_id", Cell::Id(sub_partner))
                                    .set_cell("name", Cell::Str("IC AR".into()))
                                    .set_cell("debit", Cell::Decimal(dec("2000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(p_rev))
                                    .set_cell("partner_id", Cell::Id(sub_partner))
                                    .set_cell("name", Cell::Str("IC Mgmt Fee Rev".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("2000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    ic_p.call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    let ic_s = co_env(&tx, sub_co)
        .records("account.move")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("move_type", Cell::Str("entry".into()))
                    .set_cell("journal_id", Cell::Id(j_s))
                    .set_cell("date", Cell::Date(d(2026, 6, 20)))
                    .set_commands(
                        "line_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_exp))
                                    .set_cell("partner_id", Cell::Id(parent_partner))
                                    .set_cell("name", Cell::Str("IC Mgmt Fee Exp".into()))
                                    .set_cell("debit", Cell::Decimal(dec("30000000.00")))
                                    .set_cell("credit", Cell::Decimal(Decimal::ZERO)),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("account_id", Cell::Id(s_ap))
                                    .set_cell("partner_id", Cell::Id(parent_partner))
                                    .set_cell("name", Cell::Str("IC AP".into()))
                                    .set_cell("debit", Cell::Decimal(Decimal::ZERO))
                                    .set_cell("credit", Cell::Decimal(dec("30000000.00"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    ic_s.call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap();

    // Create Consolidation Group (80% ownership on IDR Subsidiary)
    let mut ctx_all = Context::new();
    ctx_all.set(
        "allowed_company_ids",
        Value::list([Value::Int(parent_co), Value::Int(sub_co)]),
    );
    let env_all = tx.env().with_context_replaced(ctx_all);

    let grp = env_all
        .records("account.consolidation.group")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("IndoERP Group".into()))
                    .set_cell("code", Cell::Str("GRP-TEST".into()))
                    .set_cell("currency_id", Cell::Id(usd))
                    .set_cell("closing_rate_type_code", Cell::Str("closing".into()))
                    .set_cell(
                        "average_rate_type_code",
                        Cell::Str("monthly_average".into()),
                    )
                    .set_cell("historical_rate_type_code", Cell::Str("historical".into()))
                    .set_cell("fallback_to_spot", Cell::Bool(true))
                    .set_commands(
                        "unit_ids",
                        vec![
                            Command::create(
                                RawVals::new()
                                    .set_cell("company_id", Cell::Id(parent_co))
                                    .set_cell("is_parent", Cell::Bool(true))
                                    .set_cell("consolidation_method", Cell::Str("full".into()))
                                    .set_cell("ownership_pct", Cell::Decimal(dec("100"))),
                            ),
                            Command::create(
                                RawVals::new()
                                    .set_cell("company_id", Cell::Id(sub_co))
                                    .set_cell("is_parent", Cell::Bool(false))
                                    .set_cell("consolidation_method", Cell::Str("full".into()))
                                    .set_cell("ownership_pct", Cell::Decimal(dec("80"))),
                            ),
                        ],
                    ),
            ],
        )
        .await
        .unwrap();
    grp.call::<(), ()>(&mut tx, "action_bootstrap_default_chart", ())
        .await
        .unwrap();
    grp.call::<(), ()>(&mut tx, "action_auto_map_units", ())
        .await
        .unwrap();

    let run = env_all
        .records("account.consolidation.run")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("H1 2026 Consolidation".into()))
                    .set_cell("group_id", Cell::Id(grp.ids()[0]))
                    .set_cell("date_from", Cell::Date(d(2026, 1, 1)))
                    .set_cell("date_to", Cell::Date(d(2026, 6, 30))),
            ],
        )
        .await
        .unwrap();
    run.call::<(), ()>(&mut tx, "action_execute_run", ())
        .await
        .unwrap();

    let diff = run.cell(&mut tx, "balance_check_diff").await.unwrap();
    let cta = run.cell(&mut tx, "total_cta").await.unwrap();
    let nci_eq = run.cell(&mut tx, "total_nci_equity").await.unwrap();
    let nci_pl = run.cell(&mut tx, "total_nci_profit").await.unwrap();
    assert_eq!(
        diff,
        Cell::Decimal(Decimal::ZERO),
        "Consolidated Balance Sheet must balance to exact 0.00"
    );
    assert_ne!(
        cta,
        Cell::Decimal(Decimal::ZERO),
        "Multi-rate USD/IDR translation must generate non-zero CTA"
    );
    assert_ne!(
        nci_eq,
        Cell::Decimal(Decimal::ZERO),
        "80% owned subsidiary must carve out 20% NCI equity"
    );
    assert_ne!(
        nci_pl,
        Cell::Decimal(Decimal::ZERO),
        "80% owned subsidiary must carve out 20% NCI profit"
    );

    let ic_lines: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_consolidation_line WHERE run_id = $1 AND posting_level = '20_ic_elimination'",
            &[&run.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(ic_lines > 0, "Expected automatic IC elimination lines");

    // Lock period -> re-execution must be refused.
    run.call::<(), ()>(&mut tx, "action_lock_run", ())
        .await
        .unwrap();
    let relock_err = tx
        .savepoint(async |tx| run.call::<(), ()>(tx, "action_execute_run", ()).await)
        .await
        .unwrap_err();
    assert!(
        relock_err.message().contains("locked"),
        "Expected locked run refusal, got: {}",
        relock_err.message()
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
