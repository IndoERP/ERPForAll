// SPDX-License-Identifier: MIT OR Apache-2.0
//! Journals and journal groups of the `account` addon, on a full install: the
//! journal rules read the declared settings, which need the installed groups.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::{Path, PathBuf};

use erp_addon_account::journal::settings::{
    ACCOUNT_TYPES, CODE_MAX_LENGTH, account_types_key, prefix_key,
};
use erp_addon_account::journal::{AccountJournal, DEFAULT_ACCOUNT_TASK, JournalSequence};
use erp_addon_base::ir_config_setting::{read_values, write_values};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::prelude::*;
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{Command, Context, ErrorKind, FieldType, Orm, SUPERUSER_ID};
use erp_testkit::TestDb;

/// Run a test future on a thread with a 16 MiB stack: the install and the journal operations make
/// a future that overflows the default 2 MiB test-thread stack in a debug build.
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
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("account").unwrap()]))
            .await
            .unwrap();
    (db, outcome.orm)
}

async fn id_of(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}

async fn main_company(tx: &mut Transaction) -> i64 {
    id_of(
        tx,
        "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = 'main_company'",
    )
    .await
}

async fn currency(tx: &mut Transaction, name: &str) -> i64 {
    id_of(
        tx,
        &format!("SELECT id::int8 FROM res_currency WHERE name = '{name}'"),
    )
    .await
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn records(tx: &Transaction, company: i64, model: &str) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records(model)
        .unwrap()
}

async fn create(model: &Records, tx: &mut Transaction, vals: Vec<RawVals>) -> Result<Records> {
    let model = model.clone();
    tx.savepoint(async move |tx| model.create(tx, vals).await)
        .await
}

async fn write(record: &Records, tx: &mut Transaction, vals: RawVals) -> Result<()> {
    let record = record.clone();
    tx.savepoint(async move |tx| record.write(tx, vals).await)
        .await
}

/// An account of `company`; `currency` is its foreign currency.
async fn account(
    tx: &mut Transaction,
    company: i64,
    code: &str,
    account_type: &str,
    currency: Option<i64>,
) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(&format!("Account {code}")))
        .set_cell("code", text(code))
        .set_cell("account_type", text(account_type));
    if let Some(currency) = currency {
        vals = vals.set_cell("currency_id", Cell::Id(currency));
    }
    let model = records(tx, company, "account.account");
    create(&model, tx, vec![vals]).await.unwrap().ids()[0]
}

fn journal_vals(name: &str, journal_type: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", text(name))
        .set_cell("type", text(journal_type))
}

async fn str_cell(record: &Records, tx: &mut Transaction, field: &str) -> String {
    match record.cell(tx, field).await.unwrap() {
        Cell::Str(value) => value.to_string(),
        other => panic!("{field} is {other:?}"),
    }
}

/// Change one global setting with the framework's rights.
async fn set(tx: &mut Transaction, key: &str, value: Value) {
    let env = tx.env().sudo(true);
    let read = read_values(&env, tx, SettingsScope::Global {}, vec![key.to_owned()])
        .await
        .unwrap();
    let revision = erp_sdk::serde_json::to_value(&read).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    write_values(
        &env,
        tx,
        SettingsScope::Global {},
        revision,
        vec![(key.to_owned(), value)],
    )
    .await
    .unwrap();
}

fn assert_refused(result: Result<impl std::fmt::Debug>, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert!(
        error.message().contains(message),
        "expected {message:?} in {:?}",
        error.message()
    );
}

#[test]
fn codes_are_unique_per_company_and_proposed_per_type() {
    big_stack(codes_are_unique_per_company_and_proposed_per_type_body);
}

async fn codes_are_unique_per_company_and_proposed_per_type_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let journals = records(&tx, main, "account.journal");

    let created = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Sales", "sale"),
            journal_vals("Sales 2", "sale"),
            journal_vals("Purchases", "purchase"),
            journal_vals("Miscellaneous", "general"),
        ],
    )
    .await
    .unwrap();
    let codes = created.cells(&mut tx, "code").await.unwrap();
    assert_eq!(
        codes,
        [text("INV1"), text("INV2"), text("BILL1"), text("MISC1")]
    );
    // the company defaults to the active one
    assert_eq!(
        created
            .browse(vec![created.ids()[0]])
            .unwrap()
            .cell(&mut tx, "company_id")
            .await
            .unwrap(),
        Cell::Id(main)
    );
    // the prefix is a setting
    set(&mut tx, &prefix_key("sale"), Value::str("FAC")).await;
    let fac = create(&journals, &mut tx, vec![journal_vals("Invoices", "sale")])
        .await
        .unwrap();
    assert_eq!(str_cell(&fac, &mut tx, "code").await, "FAC1");

    let duplicate = create(
        &journals,
        &mut tx,
        vec![journal_vals("Other", "sale").set_cell("code", text("INV1"))],
    )
    .await;
    assert_refused(duplicate, "Journal codes must be unique per company.");

    let usd = currency(&mut tx, "USD").await;
    let other = records(&tx, main, "res.company");
    let other = create(
        &other,
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("name", text("Second Company"))
                .set_cell("currency_id", Cell::Id(usd)),
        ],
    )
    .await
    .unwrap()
    .ids()[0];
    create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Sales B", "sale")
                .set_cell("code", text("INV1"))
                .set_cell("company_id", Cell::Id(other)),
        ],
    )
    .await
    .unwrap();

    // company consistency: a journal of the second company cannot default to an
    // account of the main company only
    let income = account(&mut tx, main, "400000", "income", None).await;
    let foreign = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Sales C", "sale")
                .set_cell("company_id", Cell::Id(other))
                .set_cell("default_account_id", Cell::Id(income)),
        ],
    )
    .await;
    let error = foreign.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert!(
        error.message().contains("no company crossover is allowed")
            && error.message().contains("Default Account")
            && error.message().contains("Second Company"),
        "a cross-company default account is refused naming the field and the company: {error:?}"
    );

    // a company given as a plain integer is that company; any other kind is refused by name
    let int_company = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Sales D", "sale")
                .set_cell("company_id", Cell::Int(i32::try_from(other).unwrap())),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        int_company.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(other)
    );
    let text_company = create(
        &journals,
        &mut tx,
        vec![journal_vals("Sales E", "sale").set_cell("company_id", text("Second Company"))],
    )
    .await;
    assert_refused(text_company, "The company of a journal is a company id");

    // the allowed-types settings know every account type, no more
    let spec = orm.registry().model("account.account").unwrap();
    let FieldType::Selection {
        values: Some(values),
        ..
    } = &spec.field("account_type").unwrap().ty
    else {
        panic!("account_type is a static selection");
    };
    let keys: Vec<&str> = values.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ACCOUNT_TYPES);
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn code_length_is_bounded_by_its_setting() {
    big_stack(code_length_is_bounded_by_its_setting_body);
}

async fn code_length_is_bounded_by_its_setting_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let journals = records(&tx, main, "account.journal");
    create(
        &journals,
        &mut tx,
        vec![journal_vals("Long", "general").set_cell("code", text("ABCDEFGHIJKLMNOP"))],
    )
    .await
    .unwrap();
    let long = create(
        &journals,
        &mut tx,
        vec![journal_vals("Too long", "general").set_cell("code", text("ABCDEFGHIJKLMNOPQ"))],
    )
    .await;
    assert_refused(long, CODE_MAX_LENGTH);
    let blank = create(
        &journals,
        &mut tx,
        vec![journal_vals("Blank", "general").set_cell("code", text("  "))],
    )
    .await;
    assert_refused(blank, "starts or ends with a space");
    let empty = create(
        &journals,
        &mut tx,
        vec![journal_vals("Empty", "general").set_cell("code", text(""))],
    )
    .await;
    // an empty code is proposed instead, like a missing one
    assert_eq!(str_cell(&empty.unwrap(), &mut tx, "code").await, "MISC1");
    for padded in [" INV1", "INV1 "] {
        let refused = create(
            &journals,
            &mut tx,
            vec![journal_vals("Padded", "sale").set_cell("code", text(padded))],
        )
        .await;
        assert_refused(refused, "starts or ends with a space");
    }
    // the same rules on a write of the code
    let long = journals
        .search(
            &mut tx,
            &Domain::condition("code", Operator::Eq, "ABCDEFGHIJKLMNOP").unwrap(),
        )
        .await
        .unwrap();
    let longer = write(
        &long,
        &mut tx,
        RawVals::new().set_cell("code", text("ABCDEFGHIJKLMNOPQ")),
    )
    .await;
    assert_refused(longer, CODE_MAX_LENGTH);
    let spaced = write(
        &long,
        &mut tx,
        RawVals::new().set_cell("code", text("ABC ")),
    )
    .await;
    assert_refused(spaced, "starts or ends with a space");
    // a shorter limit refuses a proposed code that does not fit
    set(&mut tx, CODE_MAX_LENGTH, Value::Int(4)).await;
    let proposed = create(&journals, &mut tx, vec![journal_vals("Bills", "purchase")]).await;
    assert_refused(proposed, "No free journal code starting with BILL");
    // and the setting refuses a value outside its bounds
    let env = tx.env().sudo(true);
    let outside = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        "1".into(),
        vec![(CODE_MAX_LENGTH.to_owned(), Value::Int(0))],
    )
    .await;
    assert_refused(
        outside,
        &format!("{CODE_MAX_LENGTH} takes a whole number from 1 to 32"),
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn default_account_types_follow_the_journal_type() {
    big_stack(default_account_types_follow_the_journal_type_body);
}

async fn default_account_types_follow_the_journal_type_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let journals = records(&tx, main, "account.journal");
    let receivable = account(&mut tx, main, "121000", "asset_receivable", None).await;
    let income = account(&mut tx, main, "400000", "income", None).await;
    let current = account(&mut tx, main, "110000", "asset_current", None).await;

    let refused = create(
        &journals,
        &mut tx,
        vec![journal_vals("Sales", "sale").set_cell("default_account_id", Cell::Id(receivable))],
    )
    .await;
    assert_refused(
        refused,
        "The type of the journal's default credit/debit account shouldn't be 'receivable' or 'payable'.",
    );
    let sales = create(
        &journals,
        &mut tx,
        vec![journal_vals("Sales", "sale").set_cell("default_account_id", Cell::Id(income))],
    )
    .await
    .unwrap();
    let accounts = records(&tx, main, "account.account");
    let income_account = accounts.browse(vec![income]).unwrap();
    let changed = write(
        &income_account,
        &mut tx,
        RawVals::new().set_cell("account_type", text("asset_receivable")),
    )
    .await;
    assert_refused(
        changed,
        "The account is already in use in a 'sale' or 'purchase' journal.",
    );

    let key = account_types_key("sale");
    let outside = create(
        &journals,
        &mut tx,
        vec![journal_vals("Sales 2", "sale").set_cell("default_account_id", Cell::Id(current))],
    )
    .await;
    assert_refused(outside, &key);
    let to_expense = write(
        &income_account,
        &mut tx,
        RawVals::new().set_cell("account_type", text("expense")),
    )
    .await;
    assert_refused(to_expense, &key);
    // the journal side of a write: a type the account's type does not suit
    let to_purchase = write(
        &sales,
        &mut tx,
        RawVals::new().set_cell("type", text("purchase")),
    )
    .await;
    assert_refused(to_purchase, &account_types_key("purchase"));
    let to_current = write(
        &sales,
        &mut tx,
        RawVals::new().set_cell("default_account_id", Cell::Id(current)),
    )
    .await;
    assert_refused(to_current, &key);
    set(
        &mut tx,
        &key,
        Value::str("income,income_other,asset_current"),
    )
    .await;
    create(
        &journals,
        &mut tx,
        vec![journal_vals("Sales 2", "sale").set_cell("default_account_id", Cell::Id(current))],
    )
    .await
    .unwrap();
    // a list with an unknown type is refused by the setting itself
    let env = tx.env().sudo(true);
    let read = read_values(&env, &mut tx, SettingsScope::Global {}, vec![key.clone()])
        .await
        .unwrap();
    let revision = erp_sdk::serde_json::to_value(&read).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let unknown = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        revision,
        vec![(key.clone(), Value::str("income,no_such_type"))],
    )
    .await;
    assert_refused(unknown, "\"no_such_type\" is not an account type");
    // an empty list is refused as such
    let read = read_values(&env, &mut tx, SettingsScope::Global {}, vec![key.clone()])
        .await
        .unwrap();
    let revision = erp_sdk::serde_json::to_value(&read).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let empty = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        revision,
        vec![(key.clone(), Value::str(""))],
    )
    .await;
    assert_refused(empty, "List at least one account type");
    assert_eq!(sales.len(), 1);
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn journal_and_default_account_agree_on_the_foreign_currency() {
    big_stack(journal_and_default_account_agree_on_the_foreign_currency_body);
}

async fn journal_and_default_account_agree_on_the_foreign_currency_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let (eur, gbp) = (
        currency(&mut tx, "EUR").await,
        currency(&mut tx, "GBP").await,
    );
    let journals = records(&tx, main, "account.journal");

    // a bank journal needs its default account until the account is created for it
    let no_account = create(&journals, &mut tx, vec![journal_vals("Bank", "bank")]).await;
    assert_refused(no_account, DEFAULT_ACCOUNT_TASK);

    // ... also when a journal becomes a bank journal, or a bank journal loses its account
    let general = create(&journals, &mut tx, vec![journal_vals("Misc", "general")])
        .await
        .unwrap();
    let to_bank = write(
        &general,
        &mut tx,
        RawVals::new().set_cell("type", text("bank")),
    )
    .await;
    assert_refused(to_bank, DEFAULT_ACCOUNT_TASK);

    let cash = account(&mut tx, main, "101000", "asset_cash", None).await;
    let bank = create(
        &journals,
        &mut tx,
        vec![journal_vals("Bank", "bank").set_cell("default_account_id", Cell::Id(cash))],
    )
    .await
    .unwrap();
    write(
        &bank,
        &mut tx,
        RawVals::new().set_cell("currency_id", Cell::Id(eur)),
    )
    .await
    .unwrap();
    let cleared = write(
        &bank,
        &mut tx,
        RawVals::new().set_cell("default_account_id", Cell::Null),
    )
    .await;
    assert_refused(cleared, DEFAULT_ACCOUNT_TASK);
    let accounts = records(&tx, main, "account.account");
    let cash_account = accounts.browse(vec![cash]).unwrap();
    assert_eq!(
        cash_account.cell(&mut tx, "currency_id").await.unwrap(),
        Cell::Id(eur)
    );

    let changed = write(
        &cash_account,
        &mut tx,
        RawVals::new().set_cell("currency_id", Cell::Id(gbp)),
    )
    .await;
    assert_refused(
        changed,
        "The foreign currency set on the journal 'Bank' and the account 'Account 101000' must be the same.",
    );

    let pounds = account(&mut tx, main, "110000", "asset_current", Some(gbp)).await;
    let created = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Misc EUR", "general")
                .set_cell("currency_id", Cell::Id(eur))
                .set_cell("default_account_id", Cell::Id(pounds)),
        ],
    )
    .await;
    assert_refused(created, "must be the same");
    let misc = create(
        &journals,
        &mut tx,
        vec![journal_vals("Misc EUR", "general").set_cell("currency_id", Cell::Id(eur))],
    )
    .await
    .unwrap();
    let moved = write(
        &misc,
        &mut tx,
        RawVals::new().set_cell("default_account_id", Cell::Id(pounds)),
    )
    .await;
    assert_refused(moved, "must be the same");
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn journal_groups_have_unique_names_and_list_their_journals() {
    big_stack(journal_groups_have_unique_names_and_list_their_journals_body);
}

async fn journal_groups_have_unique_names_and_list_their_journals_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let groups = records(&tx, main, "account.journal.group");
    let ifrs = create(
        &groups,
        &mut tx,
        vec![RawVals::new().set_cell("name", text("IFRS"))],
    )
    .await
    .unwrap();
    assert_eq!(ifrs.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(10));
    let twice = create(
        &groups,
        &mut tx,
        vec![RawVals::new().set_cell("name", text("IFRS"))],
    )
    .await;
    assert_refused(twice, "A Ledger group name must be unique.");
    let journals = records(&tx, main, "account.journal");
    let adjustments = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("IFRS adjustments", "general")
                .set_cell("journal_group_id", Cell::Id(ifrs.ids()[0])),
        ],
    )
    .await
    .unwrap();
    // archived journals stay in the group's list
    write(
        &adjustments,
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    match ifrs.cell(&mut tx, "included_journal_ids").await.unwrap() {
        Cell::Ids(ids) => assert_eq!(
            ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
            adjustments.ids()
        ),
        other => panic!("included_journal_ids is {other:?}"),
    }
    // one can
    let ifrs_again = ifrs.clone();
    let deleted = tx
        .savepoint(async move |tx| ifrs_again.unlink(tx).await)
        .await;
    assert_refused(
        deleted,
        "Another model is using the record you are trying to delete.\n\n\
         The troublemaker is: 'Journal' (account.journal)",
    );
    assert_eq!(
        adjustments.cell(&mut tx, "journal_group_id").await.unwrap(),
        Cell::Id(ifrs.ids()[0]),
        "the journal keeps its ledger"
    );
    let unused = create(
        &groups,
        &mut tx,
        vec![RawVals::new().set_cell("name", text("Tax"))],
    )
    .await
    .unwrap();
    let unused_again = unused.clone();
    tx.savepoint(async move |tx| unused_again.unlink(tx).await)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn numbering_declaration_follows_the_type_and_builds_patterns() {
    big_stack(numbering_declaration_follows_the_type_and_builds_patterns_body);
}

async fn numbering_declaration_follows_the_type_and_builds_patterns_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let journals = records(&tx, main, "account.journal");
    let cash = account(&mut tx, main, "101000", "asset_cash", None).await;
    let created = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Sales", "sale"),
            journal_vals("Miscellaneous", "general"),
            journal_vals("Bank", "bank").set_cell("default_account_id", Cell::Id(cash)),
            journal_vals("Kept", "sale")
                .set_cell("sequence_reset", text("never"))
                .set_cell("sequence_padding", Cell::Int(3))
                .set_cell("refund_sequence", Cell::Bool(false)),
        ],
    )
    .await
    .unwrap();
    let typed = tx.env().with_company(main).unwrap();
    let date = Date::from_ymd(2026, 7, 14).unwrap();
    let mut rendered = Vec::new();
    for id in created.ids() {
        let journal = typed
            .model::<AccountJournal>()
            .unwrap()
            .browse(vec![id])
            .unwrap();
        let sequence = JournalSequence::of(&mut tx, &journal).await.unwrap();
        for kind in ["document", "refund", "payment"] {
            let (chain, pattern) = sequence.pattern(kind).unwrap();
            rendered.push(format!(
                "{chain}:{}",
                pattern.render(1, date, None).unwrap()
            ));
        }
    }
    assert_eq!(
        rendered,
        [
            "document:INV1/2026/00001",
            "refund:RINV1/2026/00001",
            "document:INV1/2026/00001",
            "document:MISC1/2026/07/0001",
            "document:MISC1/2026/07/0001",
            "document:MISC1/2026/07/0001",
            "document:BNK1/2026/00001",
            "document:BNK1/2026/00001",
            "payment:PBNK1/2026/00001",
            "document:INV2/001",
            "document:INV2/001",
            "document:INV2/001",
        ]
    );
    // a type change keeps the administrator's numbering; only the separate chains follow the type
    let kept = typed
        .model::<AccountJournal>()
        .unwrap()
        .browse(vec![created.ids()[3]])
        .unwrap();
    write(
        kept.as_records(),
        &mut tx,
        RawVals::new().set_cell("type", text("general")),
    )
    .await
    .unwrap();
    let sequence = JournalSequence::of(&mut tx, &kept).await.unwrap();
    assert_eq!(
        sequence
            .pattern("document")
            .unwrap()
            .1
            .render(1, date, None)
            .unwrap(),
        "INV2/001"
    );
    // a written code or numbering that cannot make a pattern is refused when saved
    for vals in [
        RawVals::new().set_cell("code", text("A\nB")),
        RawVals::new().set_cell("sequence_padding", Cell::Int(0)),
        RawVals::new().set_cell("sequence_padding", Cell::Int(65)),
    ] {
        let refused = write(kept.as_records(), &mut tx, vals).await;
        assert!(refused.is_err(), "refused");
        let error = refused.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    }
    // the counter width is bounded
    for padding in [0, 65] {
        let refused = create(
            &journals,
            &mut tx,
            vec![journal_vals("Bad", "general").set_cell("sequence_padding", Cell::Int(padding))],
        )
        .await;
        assert_refused(refused, "must be between 1 and 64");
    }
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

/// A company of the database; `parent` makes it a branch.
async fn company(tx: &mut Transaction, main: i64, name: &str, parent: Option<i64>) -> i64 {
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
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("currency_id", Cell::Id(currency));
    if let Some(parent) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(parent));
    }
    let model = records(tx, main, "res.company");
    create(&model, tx, vec![vals]).await.unwrap().ids()[0]
}

/// An account of `account_type` shared by `companies`.
async fn shared_account(
    tx: &mut Transaction,
    main: i64,
    code: &str,
    account_type: &str,
    companies: &[i64],
) -> Records {
    let vals = RawVals::new()
        .set_cell("name", text(&format!("Account {code}")))
        .set_cell("code", text(code))
        .set_cell("account_type", text(account_type))
        .set_commands(
            "company_ids",
            vec![Command::set(
                companies.iter().map(|id| RecId::from_real(*id).unwrap()),
            )],
        );
    let model = records(tx, main, "account.account");
    create(&model, tx, vec![vals]).await.unwrap()
}

fn only(companies: &[i64]) -> RawVals {
    RawVals::new().set_commands(
        "company_ids",
        vec![Command::set(
            companies.iter().map(|id| RecId::from_real(*id).unwrap()),
        )],
    )
}

#[test]
fn an_account_keeps_the_companies_of_the_journals_using_it() {
    big_stack(an_account_keeps_the_companies_of_the_journals_using_it_body);
}

async fn an_account_keeps_the_companies_of_the_journals_using_it_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let second = company(&mut tx, main, "Second Company", None).await;
    let branch = company(&mut tx, main, "Main Branch", Some(main)).await;
    let journals = records(&tx, main, "account.journal");

    // each account field of a journal of the second company: the account cannot drop that company
    let cases = [
        ("default_account_id", "Default Account", "sale", "income"),
        (
            "suspense_account_id",
            "Suspense Account",
            "general",
            "asset_current",
        ),
        ("profit_account_id", "Profit Account", "general", "income"),
        ("loss_account_id", "Loss Account", "general", "expense"),
    ];
    for (n, (field, label, journal_type, account_type)) in cases.into_iter().enumerate() {
        let code = format!("40{n}000");
        let account = shared_account(&mut tx, main, &code, account_type, &[main, second]).await;
        create(
            &journals,
            &mut tx,
            vec![
                journal_vals(&format!("Second {label}"), journal_type)
                    .set_cell("company_id", Cell::Id(second))
                    .set_cell(field, Cell::Id(account.ids()[0])),
            ],
        )
        .await
        .unwrap();
        let narrowed = write(&account, &mut tx, only(&[main])).await;
        assert_refused(
            narrowed,
            &format!(
                "The account 'Account {code}' is the {label} of the journal 'Second {label}' of a \
                 company that is not among the account's companies"
            ),
        );
        // keeping the journal's company is allowed
        write(&account, &mut tx, only(&[second])).await.unwrap();
        // and the account cannot be deleted while the journal uses it
        let used = account.clone();
        let deleted = tx.savepoint(async move |tx| used.unlink(tx).await).await;
        assert_refused(deleted, "The troublemaker is: 'Journal' (account.journal)");
    }

    // a journal of a branch may use an account of its parent company only
    let account = shared_account(&mut tx, main, "410000", "income", &[main, second]).await;
    create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Branch Sales", "sale")
                .set_cell("company_id", Cell::Id(branch))
                .set_cell("default_account_id", Cell::Id(account.ids()[0])),
        ],
    )
    .await
    .unwrap();
    write(&account, &mut tx, only(&[main])).await.unwrap();
    // but not of an unrelated company
    let refused = write(&account, &mut tx, only(&[second])).await;
    assert_refused(
        refused,
        "is the Default Account of the journal 'Branch Sales'",
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

/// An ORM with the access policy compiled from the database as it is now (the users created since
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

async fn group(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    id_of(
        tx,
        &format!(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = '{module}' AND name = '{name}'"
        ),
    )
    .await
}

/// An accounting administrator of `company` only.
async fn accountant(tx: &mut Transaction, main: i64, login: &str, company: i64) -> i64 {
    let manager = group(tx, "account", "group_account_manager").await;
    user(tx, main, login, company, manager).await
}

/// An internal user of `company` only, also in `extra`.
async fn user(tx: &mut Transaction, main: i64, login: &str, company: i64, extra: i64) -> i64 {
    let internal = group(tx, "base", "group_user").await;
    let users = records(tx, main, "res.users");
    let vals = RawVals::new()
        .set_cell("name", text(login))
        .set_cell("login", text(login))
        .set_cell("password", text("journal-fixture-password"))
        .set_cell("company_id", Cell::Id(company))
        .set_commands(
            "company_ids",
            vec![Command::set([RecId::from_real(company).unwrap()])],
        )
        .set_commands(
            "group_ids",
            vec![Command::set([
                RecId::from_real(extra).unwrap(),
                RecId::from_real(internal).unwrap(),
            ])],
        );
    create(&users, tx, vec![vals]).await.unwrap().ids()[0]
}

async fn visible_codes(tx: &mut Transaction, company: i64) -> Vec<String> {
    let journals = tx
        .env()
        .with_company(company)
        .unwrap()
        .records("account.journal")
        .unwrap()
        .with_context([("active_test", Value::Bool(false))]);
    let found = journals.search(tx, &Domain::TRUE).await.unwrap();
    let mut codes: Vec<String> = found
        .cells(tx, "code")
        .await
        .unwrap()
        .into_iter()
        .map(|cell| match cell {
            Cell::Str(code) => code.to_string(),
            other => panic!("code is {other:?}"),
        })
        .collect();
    codes.sort();
    codes
}

#[test]
fn journals_follow_the_company_rule_of_real_users() {
    big_stack(journals_follow_the_company_rule_of_real_users_body);
}

async fn journals_follow_the_company_rule_of_real_users_body() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let second = company(&mut tx, main, "Second Company", None).await;
    let branch = company(&mut tx, main, "Main Branch", Some(main)).await;
    let journals = records(&tx, main, "account.journal");
    let main_sales = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Main Sales", "sale"),
            journal_vals("Main Old Sales", "sale"),
        ],
    )
    .await
    .unwrap();
    let old = main_sales.browse(vec![main_sales.ids()[1]]).unwrap();
    write(
        &old,
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let shared = shared_account(&mut tx, main, "400000", "income", &[main, second]).await;
    let second_sales = create(
        &journals,
        &mut tx,
        vec![
            journal_vals("Second Sales", "sale")
                .set_cell("company_id", Cell::Id(second))
                .set_cell("default_account_id", Cell::Id(shared.ids()[0])),
        ],
    )
    .await
    .unwrap()
    .ids()[0];
    // the database keeps an active settings administrator
    let system = group(&mut tx, "base", "group_system").await;
    user(&mut tx, main, "journal-admin", main, system).await;
    let main_user = accountant(&mut tx, main, "main-accountant", main).await;
    let second_user = accountant(&mut tx, main, "second-accountant", second).await;
    let branch_user = accountant(&mut tx, main, "branch-accountant", branch).await;
    tx.commit().await.unwrap();
    let orm = policy_orm(&db, &orm).await;

    // the administrator of the second company sees and creates only its journals
    let mut tx = orm
        .begin(erp_orm::UserId::new(second_user).unwrap())
        .await
        .unwrap();
    assert_eq!(visible_codes(&mut tx, second).await, ["INV1"]);
    let own = records(&tx, second, "account.journal");
    let created = create(&own, &mut tx, vec![journal_vals("Second Sales 2", "sale")])
        .await
        .unwrap();
    assert_eq!(
        created.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(second)
    );
    assert_eq!(str_cell(&created, &mut tx, "code").await, "INV2");
    let foreign = create(
        &own,
        &mut tx,
        vec![journal_vals("Into Main", "sale").set_cell("company_id", Cell::Id(main))],
    )
    .await;
    assert!(
        foreign.is_err(),
        "a journal of a company the user lacks is refused"
    );
    tx.rollback().await.unwrap();

    // a branch user sees the parent company's journals, archived ones included, and the proposed
    // code skips their codes
    let mut tx = orm
        .begin(erp_orm::UserId::new(branch_user).unwrap())
        .await
        .unwrap();
    assert_eq!(visible_codes(&mut tx, branch).await, ["INV1", "INV2"]);
    let own = records(&tx, branch, "account.journal");
    let created = create(&own, &mut tx, vec![journal_vals("Branch Sales", "sale")])
        .await
        .unwrap();
    assert_eq!(str_cell(&created, &mut tx, "code").await, "INV3");
    tx.rollback().await.unwrap();

    // an administrator of the main company only cannot take the second company off the shared
    // account: the write keeps the companies it cannot see, so the second company's journal keeps
    // a company of its account (the refusal covers writers who see both)
    let mut tx = orm
        .begin(erp_orm::UserId::new(main_user).unwrap())
        .await
        .unwrap();
    let account = records(&tx, main, "account.account")
        .browse(shared.ids())
        .unwrap();
    write(&account, &mut tx, only(&[main])).await.unwrap();
    let kept: Vec<i64> = tx
        .query(
            "SELECT res_company_id::int8 FROM account_account_res_company_rel \
             WHERE account_account_id = $1 ORDER BY 1",
            &[&shared.ids()[0]],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    let mut expected = vec![main, second];
    expected.sort_unstable();
    assert_eq!(kept, expected, "the hidden company stays on the account");
    assert!(second_sales > 0);
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn concurrent_proposals_of_one_code_end_in_a_named_conflict() {
    big_stack(concurrent_proposals_of_one_code_end_in_a_named_conflict_body);
}

async fn concurrent_proposals_of_one_code_end_in_a_named_conflict_body() {
    let (db, orm) = install().await;
    let mut first = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut first).await;
    let journals = records(&first, main, "account.journal");
    let won = journals
        .create(&mut first, vec![journal_vals("First", "sale")])
        .await
        .unwrap();
    assert_eq!(str_cell(&won, &mut first, "code").await, "INV1");
    let mut second = orm.begin(SUPERUSER_ID).await.unwrap();
    let other = records(&second, main, "account.journal");
    let watcher = db.client().await.unwrap();
    let (lost, ()) = tokio::join!(
        other.create(&mut second, vec![journal_vals("Second", "sale")]),
        async move {
            // commit the first only once the second waits on the unique key
            for _ in 0..100_000 {
                let waiting: i64 = watcher
                    .query_one(
                        "SELECT count(*) FROM pg_stat_activity \
                         WHERE datname = current_database() AND wait_event_type = 'Lock'",
                        &[],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            first.commit().await.unwrap();
        }
    );
    let error = lost.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error:?}");
    assert!(
        error
            .message()
            .contains("took the proposed code at the same time"),
        "{error:?}"
    );
    drop(second);
    db.cleanup().await.unwrap();
}
