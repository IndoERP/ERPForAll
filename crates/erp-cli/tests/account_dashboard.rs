// SPDX-License-Identifier: MIT OR Apache-2.0
//! The accounting dashboard of the `account` addon, on a full install: the card JSON of each
//! journal type, its access and company scope, its two settings and the dashboard view, action
//! and menu.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::{Path, PathBuf};

use erp_addon_account::journal::dashboard::{BALANCE_DAYS, GRAPH_MONTHS};
use erp_addon_base::ir_config_setting::write_values;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Value as Json};
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{Command, Context, ErrorKind, Orm, SUPERUSER_ID};
use erp_testkit::TestDb;

/// Run a test future on a thread with a 16 MiB stack (the install overflows the default one).
fn big_stack<F: std::future::Future<Output = ()> + 'static>(test: fn() -> F) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

/// A fresh database with `account` installed.
async fn install() -> (TestDb, Orm) {
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
        .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
        .require_sql_uninstall_guards([&erp_addon_account::reconcile::history_guard::UNINSTALL])
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
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
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("account").unwrap()]))
            .await
            .unwrap();
    (db, outcome.orm)
}

/// An ORM with the access policy compiled from the database as it is now (users created since
/// the install included), as the server builds it.
async fn policy_orm(db: &TestDb, installed: &Orm) -> Orm {
    let source = erp_security::source::PolicySource::build(
        std::sync::Arc::clone(installed.registry()),
        db.pool().clone(),
    )
    .await
    .unwrap();
    Orm::builder(
        std::sync::Arc::clone(installed.registry()),
        db.pool().clone(),
    )
    .generation_attachment(erp_orm::GenerationAttachment::new(
        std::sync::Arc::clone(installed.registry()),
        installed
            .env(SUPERUSER_ID, Context::new())
            .generation_attachment::<erp_views::PublishedViewTypes>()
            .unwrap(),
    ))
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .resolver(erp_security::resolver::Resolver::for_registry(installed.registry()).unwrap())
    .policy(std::sync::Arc::new(source) as std::sync::Arc<dyn erp_orm::PolicyProvider>)
    .impls(erp_modules::base_models::IMPLS.iter().copied())
    .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .unwrap()
}

async fn id_of(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}

async fn xml_id(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    id_of(
        tx,
        &format!(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = '{module}' AND name = '{name}'"
        ),
    )
    .await
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn dec(value: i64) -> Cell {
    Cell::Decimal(Decimal::from(value))
}

fn records(tx: &Transaction, company: i64, model: &str) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records(model)
        .unwrap()
}

async fn create(model: &Records, tx: &mut Transaction, vals: Vec<RawVals>) -> Records {
    let model = model.clone();
    tx.savepoint(async move |tx| model.create(tx, vals).await)
        .await
        .unwrap()
}

/// A second root company with the main company's currency.
async fn company(tx: &mut Transaction, main: i64, name: &str) -> i64 {
    let currency = match records(tx, main, "res.company")
        .browse(vec![main])
        .unwrap()
        .cell(tx, "currency_id")
        .await
        .unwrap()
    {
        Cell::Id(id) => id,
        other => panic!("currency_id is {other:?}"),
    };
    let vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("currency_id", Cell::Id(currency));
    create(&records(tx, main, "res.company"), tx, vec![vals])
        .await
        .ids()[0]
}

async fn account(tx: &mut Transaction, company: i64, code: &str, account_type: &str) -> i64 {
    let vals = RawVals::new()
        .set_cell("name", text(&format!("Account {code}")))
        .set_cell("code", text(code))
        .set_cell("account_type", text(account_type));
    create(&records(tx, company, "account.account"), tx, vec![vals])
        .await
        .ids()[0]
}

async fn journal(
    tx: &mut Transaction,
    company: i64,
    name: &str,
    journal_type: &str,
    default_account: Option<i64>,
) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("type", text(journal_type));
    if let Some(account) = default_account {
        vals = vals.set_cell("default_account_id", Cell::Id(account));
    }
    create(&records(tx, company, "account.journal"), tx, vec![vals])
        .await
        .ids()[0]
}

/// A draft entry in `journal`: `amount` debited on `debit` and credited on `credit`.
async fn entry(
    tx: &mut Transaction,
    company: i64,
    journal: i64,
    debit: i64,
    credit: i64,
    amount: i64,
) -> Records {
    let line = |account: i64, debit: i64, credit: i64| {
        let mut vals = RawVals::new().set_cell("account_id", Cell::Id(account));
        if debit != 0 {
            vals.insert("debit".into(), dec(debit));
        }
        if credit != 0 {
            vals.insert("credit".into(), dec(credit));
        }
        Command::create(vals)
    };
    let vals = RawVals::new()
        .set_cell("journal_id", Cell::Id(journal))
        .set_commands(
            "line_ids",
            vec![line(debit, amount, 0), line(credit, 0, amount)],
        );
    create(&records(tx, company, "account.move"), tx, vec![vals]).await
}

async fn card(tx: &mut Transaction, company: i64, journal: i64) -> Result<Json> {
    let record = records(tx, company, "account.journal").browse(vec![journal])?;
    match record.cell(tx, "dashboard_card").await? {
        Cell::Str(json) => Ok(serde_json::from_str(&json).unwrap()),
        other => panic!("dashboard_card is {other:?}"),
    }
}

fn stat<'a>(card: &'a Json, label: &str) -> &'a Json {
    card["stats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["label"] == label)
        .unwrap_or_else(|| panic!("no stat {label} in {card}"))
}

fn amount(value: &Json) -> Decimal {
    value.as_str().unwrap().parse().unwrap()
}

/// The accounts and journals of one company.
struct Books {
    receivable: i64,
    payable: i64,
    income: i64,
    expense: i64,
    bank_account: i64,
    sale: i64,
    purchase: i64,
    bank: i64,
    general: i64,
}

async fn books(tx: &mut Transaction, company: i64) -> Books {
    let receivable = account(tx, company, "121000", "asset_receivable").await;
    let payable = account(tx, company, "211000", "liability_payable").await;
    let income = account(tx, company, "400000", "income").await;
    let expense = account(tx, company, "600000", "expense").await;
    let bank_account = account(tx, company, "101401", "asset_cash").await;
    Books {
        receivable,
        payable,
        income,
        expense,
        bank_account,
        sale: journal(tx, company, "Dash Sales", "sale", Some(income)).await,
        purchase: journal(tx, company, "Dash Purchases", "purchase", Some(expense)).await,
        bank: journal(tx, company, "Dash Bank", "bank", Some(bank_account)).await,
        general: journal(tx, company, "Dash Misc", "general", None).await,
    }
}

#[test]
fn cards_per_journal_type_with_drafts() {
    big_stack(cards_per_journal_type_with_drafts_body);
}

async fn cards_per_journal_type_with_drafts_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = xml_id(&mut tx, "base", "main_company").await;
    let b = books(&mut tx, main).await;
    entry(&mut tx, main, b.sale, b.receivable, b.income, 100).await;
    entry(&mut tx, main, b.sale, b.income, b.receivable, 25).await;
    entry(&mut tx, main, b.purchase, b.expense, b.payable, 70).await;
    entry(&mut tx, main, b.bank, b.bank_account, b.income, 50).await;
    entry(&mut tx, main, b.general, b.expense, b.income, 10).await;
    let currency = match records(&tx, main, "res.company")
        .browse(vec![main])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap()
    {
        Cell::Id(id) => id,
        other => panic!("currency_id is {other:?}"),
    };

    // all four journals in one read: each card is its own journal's
    let all = records(&tx, main, "account.journal")
        .browse(vec![b.sale, b.purchase, b.bank, b.general])
        .unwrap();
    let cells = all.cells(&mut tx, "dashboard_card").await.unwrap();
    assert_eq!(cells.len(), 4);
    for (cell, id) in cells.iter().zip([b.sale, b.purchase, b.bank, b.general]) {
        let Cell::Str(json) = cell else {
            panic!("{cell:?}")
        };
        let card: Json = serde_json::from_str(json).unwrap();
        assert_eq!(card["v"], 1);
        if id == b.bank {
            // a bank journal starts a new bank statement from its card
            assert_eq!(card["buttons"][0]["label"], "New Statement", "{card}");
            assert_eq!(card["buttons"][0]["context"]["default_journal_id"], id);
        } else {
            assert_eq!(card["buttons"][0]["context"]["default_journal_id"], id);
        }
        for stat in card["stats"].as_array().unwrap() {
            if stat["label"] == "Balance" {
                // journal items of the default account: no journal default
                assert_eq!(stat["context"], serde_json::json!({}));
                continue;
            }
            assert_eq!(
                stat["domain"][0],
                serde_json::json!(["journal_id", "=", id])
            );
            // New in the opened list creates in this journal
            assert_eq!(
                stat["context"],
                serde_json::json!({"default_journal_id": id}),
                "{stat}"
            );
        }
    }

    // the lists the figures of a sales and a purchase card open show every document type
    for name in ["action_move_out_documents", "action_move_in_documents"] {
        let action = xml_id(&mut tx, "account", name).await;
        let rows = tx
            .query(
                "SELECT res_model, domain FROM ir_act_window WHERE id::int8 = $1",
                &[&action],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{name} is installed");
        assert_eq!(rows[0].get::<_, String>(0), "account.move");
        assert_eq!(
            rows[0].get::<_, Option<String>>(1),
            None,
            "{name} has no domain of its own"
        );
    }

    let sale = card(&mut tx, main, b.sale).await.unwrap();
    assert_eq!(sale["buttons"][0]["label"], "New Invoice");
    assert_eq!(
        sale["buttons"][0]["action"],
        "account.action_move_out_invoice_type"
    );
    assert_eq!(sale["buttons"][0]["view_mode"], "form");
    assert_eq!(
        sale["buttons"][0]["context"]["default_move_type"],
        "out_invoice"
    );
    let drafts = stat(&sale, "Drafts");
    assert_eq!(drafts["count"], 2);
    assert_eq!(amount(&drafts["amount"]), Decimal::from(75));
    assert_eq!(drafts["currency_id"], currency);
    // the invoice of 100 and the refund-like entry of 25 are both counted, so the list the
    // figure opens is not limited to one document type
    assert_eq!(drafts["action"], "account.action_move_out_documents");
    assert_eq!(drafts["context"]["default_journal_id"], b.sale);
    // a figure that counts nothing is shown at zero, in the neutral tone
    for label in ["Unpaid", "Late"] {
        let zero = stat(&sale, label);
        assert_eq!(zero["action"], "account.action_move_out_documents");
        assert_eq!(zero["count"], 0, "{sale}");
        assert_eq!(amount(&zero["amount"]), Decimal::ZERO);
        assert_eq!(zero["tone"], "neutral");
    }
    assert_eq!(sale["chart"]["type"], "bar");
    // the current month and the twelve before it, each zero without posted invoices
    let points = sale["chart"]["points"].as_array().unwrap();
    assert_eq!(points.len(), 13);
    assert!(points.iter().all(|p| amount(&p["value"]) == Decimal::ZERO));
    let today = tx.env().context_today().unwrap();
    let (year, month, _) = today.ymd();
    assert_eq!(points[12]["label"], format!("{year:04}-{month:02}"));

    // a journal without entries shows its three figures at zero and the months of its chart at zero
    let empty = journal(&mut tx, main, "Dash Empty Sales", "sale", Some(b.income)).await;
    let empty = card(&mut tx, main, empty).await.unwrap();
    let figures = empty["stats"].as_array().unwrap();
    assert_eq!(figures.len(), 3, "{empty}");
    assert!(
        figures
            .iter()
            .all(|f| f["count"] == 0 && f["tone"] == "neutral")
    );
    let months = empty["chart"]["points"].as_array().unwrap();
    assert_eq!(months.len(), 13);
    assert!(months.iter().all(|p| amount(&p["value"]) == Decimal::ZERO));

    let purchase = card(&mut tx, main, b.purchase).await.unwrap();
    assert_eq!(purchase["buttons"][0]["label"], "New Bill");
    assert_eq!(
        purchase["buttons"][0]["context"]["default_move_type"],
        "in_invoice"
    );
    let drafts = stat(&purchase, "Drafts");
    assert_eq!(drafts["count"], 1);
    assert_eq!(
        amount(&drafts["amount"]),
        Decimal::from(70),
        "purchases show a positive amount"
    );
    for label in ["Drafts", "To pay", "Late"] {
        assert_eq!(
            stat(&purchase, label)["action"],
            "account.action_move_in_documents"
        );
    }
    assert_eq!(drafts["context"]["default_journal_id"], b.purchase);

    let bank = card(&mut tx, main, b.bank).await.unwrap();
    assert_eq!(
        bank["buttons"],
        serde_json::json!([{
            "action": "account.action_bank_statement_tree",
            "context": {"default_journal_id": b.bank},
            "label": "New Statement",
            "primary": true,
            "view_mode": "form"
        }])
    );
    // the journal's entries are a figure: its count is the list it opens (one draft entry)
    let transactions = stat(&bank, "Transactions");
    assert_eq!(transactions["count"], 1, "{bank}");
    assert!(transactions["amount"].is_null() && transactions["currency_id"].is_null());
    assert_eq!(transactions["action"], "account.action_move_journal_line");
    // the list shows every kind of entry: the stat's own domain keeps the plain ones it counts
    assert_eq!(
        transactions["domain"],
        serde_json::json!([["journal_id", "=", b.bank], ["move_type", "=", "entry"]])
    );
    assert_eq!(transactions["context"]["default_journal_id"], b.bank);
    assert_eq!(stat(&bank, "Balance")["context"], serde_json::json!({}));
    // a draft is not in the balance: the figure is shown at zero
    assert_eq!(stat(&bank, "Balance")["count"], 0, "{bank}");
    assert_eq!(amount(&stat(&bank, "Balance")["amount"]), Decimal::ZERO);
    assert_eq!(bank["chart"]["type"], "line");
    assert_eq!(bank["chart"]["points"].as_array().unwrap().len(), 30);

    let general = card(&mut tx, main, b.general).await.unwrap();
    assert_eq!(general["buttons"][0]["label"], "New Entry");
    assert_eq!(
        general["buttons"][0]["context"]["default_move_type"],
        "entry"
    );
    assert_eq!(stat(&general, "Drafts")["count"], 1);
    assert_eq!(
        stat(&general, "Drafts")["action"],
        "account.action_move_journal_line"
    );
    assert_eq!(
        stat(&general, "Drafts")["context"]["default_journal_id"],
        b.general
    );
    assert!(stat(&general, "Drafts")["amount"].is_null());
    assert!(general["chart"].is_null());
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

async fn user(tx: &mut Transaction, main: i64, login: &str, company: i64, groups: &[i64]) -> i64 {
    let internal = xml_id(tx, "base", "group_user").await;
    let vals = RawVals::new()
        .set_cell("name", text(login))
        .set_cell("login", text(login))
        .set_cell("password", text("dashboard-fixture-password"))
        .set_cell("company_id", Cell::Id(company))
        .set_commands(
            "company_ids",
            vec![Command::set([RecId::from_real(company).unwrap()])],
        )
        .set_commands(
            "group_ids",
            vec![Command::set(
                groups
                    .iter()
                    .chain([&internal])
                    .map(|id| RecId::from_real(*id).unwrap()),
            )],
        );
    create(&records(tx, main, "res.users"), tx, vec![vals])
        .await
        .ids()[0]
}

#[test]
fn cards_follow_the_reader_company_and_access() {
    big_stack(cards_follow_the_reader_company_and_access_body);
}

async fn cards_follow_the_reader_company_and_access_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = xml_id(&mut tx, "base", "main_company").await;
    let second = company(&mut tx, main, "Second Company").await;
    let a = books(&mut tx, main).await;
    let b = books(&mut tx, second).await;
    entry(&mut tx, main, a.general, a.expense, a.income, 10).await;
    for _ in 0..3 {
        entry(&mut tx, second, b.general, b.expense, b.income, 10).await;
    }
    // the database keeps an active settings administrator: the rule runs on every user create
    let system = xml_id(&mut tx, "base", "group_system").await;
    user(&mut tx, main, "dashboard-admin", main, &[system]).await;
    let readonly = xml_id(&mut tx, "account", "group_account_readonly").await;
    let reader = user(&mut tx, main, "dashboard-reader", main, &[readonly]).await;
    let outsider = user(&mut tx, main, "dashboard-outsider", main, &[]).await;
    tx.commit().await.unwrap();
    let orm = policy_orm(&db, &orm).await;

    let mut tx = orm
        .begin(erp_orm::UserId::new(reader).unwrap())
        .await
        .unwrap();
    let own = card(&mut tx, main, a.general).await.unwrap();
    assert_eq!(
        stat(&own, "Drafts")["count"],
        1,
        "the other company's entries are not counted"
    );
    let visible = records(&tx, main, "account.journal")
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    assert!(
        !visible.contains(&b.general),
        "the other company's journal is not listed"
    );
    let foreign = card(&mut tx, main, b.general).await;
    assert!(
        foreign.is_err(),
        "the other company's card is refused: {foreign:?}"
    );
    tx.rollback().await.unwrap();

    let mut tx = orm
        .begin(erp_orm::UserId::new(outsider).unwrap())
        .await
        .unwrap();
    let refused = card(&mut tx, main, a.general).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Access, "{refused:?}");
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

/// The two dashboard settings refuse a value outside their bounds, by name.
#[test]
fn dashboard_settings_are_bounded() {
    big_stack(dashboard_settings_are_bounded_body);
}

async fn dashboard_settings_are_bounded_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env().sudo(true);
    for (key, value) in [
        (GRAPH_MONTHS, 0),
        (GRAPH_MONTHS, 37),
        (BALANCE_DAYS, 6),
        (BALANCE_DAYS, 93),
    ] {
        let refused = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            "1".into(),
            vec![(key.to_owned(), Value::Int(value))],
        )
        .await
        .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation, "{refused:?}");
        assert!(
            refused.message().contains(key),
            "{key}: {}",
            refused.message()
        );
    }
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn dashboard_view_action_and_menu_load() {
    big_stack(dashboard_view_action_and_menu_load_body);
}

async fn dashboard_view_action_and_menu_load_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT a.res_model, a.view_mode, a.domain, v.type, dp.name, m.sequence, \
               (SELECT min(s.sequence) FROM ir_ui_menu s WHERE s.parent_id = m.parent_id) \
             FROM ir_ui_menu m \
             JOIN ir_model_data dm ON dm.model = 'ir.ui.menu' AND dm.res_id = m.id \
             JOIN ir_model_data dp ON dp.model = 'ir.ui.menu' AND dp.res_id = m.parent_id \
             JOIN ir_model_data da ON da.model = 'ir.actions.act_window' \
              AND m.action = 'ir.actions.act_window,' || da.res_id \
             JOIN ir_act_window a ON a.id = da.res_id \
             JOIN ir_ui_view v ON v.id = a.view_id \
             WHERE dm.module = 'account' AND dm.name = 'menu_board_journal_1' \
               AND da.module = 'account' AND da.name = 'open_account_journal_dashboard_kanban'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "menu, action and view are installed");
    let row = &rows[0];
    assert_eq!(row.get::<_, String>(0), "account.journal");
    assert_eq!(row.get::<_, String>(1), "kanban,form");
    assert!(row.get::<_, String>(2).contains("show_on_dashboard"));
    assert_eq!(row.get::<_, String>(3), "kanban");
    assert_eq!(row.get::<_, String>(4), "menu_finance");
    assert_eq!(
        row.get::<_, i32>(5),
        row.get::<_, i32>(6),
        "the dashboard is the first entry of Accounting"
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}
