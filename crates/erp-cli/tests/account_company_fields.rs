// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed company accounting configuration: defaults, integrity and Settings.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "integration assertions"
)]
use erp_addon_base::ir_config_setting::{describe, read_values, write_values};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::prelude::*;
use erp_sdk::views::wire::{SettingsScope, WireId};
use erp_sdk::{Command, Context, ErrorKind, Orm, SUPERUSER_ID};
use erp_testkit::TestDb;
use std::path::{Path, PathBuf};
#[path = "support/native_http.rs"]
mod native_http;

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
async fn install() -> (TestDb, Orm, ModuleSystem) {
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
    (db, outcome.orm, system)
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

/// An internal user of `companies` (the first is its company), also in `extra`.
async fn user_of(
    tx: &mut Transaction,
    main: i64,
    login: &str,
    companies: &[i64],
    extra: i64,
) -> i64 {
    let internal = group(tx, "base", "group_user").await;
    let users = records(tx, main, "res.users");
    let vals = RawVals::new()
        .set_cell("name", text(login))
        .set_cell("login", text(login))
        .set_cell("password", text("journal-fixture-password"))
        .set_cell("company_id", Cell::Id(companies[0]))
        .set_commands(
            "company_ids",
            vec![Command::set(
                companies.iter().map(|id| RecId::from_real(*id).unwrap()),
            )],
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

async fn group(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    id_of(
        tx,
        &format!(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = '{module}' AND name = '{name}'"
        ),
    )
    .await
}

const ACCOUNT_FIELDS: [&str; 11] = [
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
];
const JOURNAL_FIELDS: [&str; 2] = [
    "currency_exchange_journal_id",
    "automatic_entry_default_journal_id",
];
const PREFIX_FIELDS: [&str; 3] = [
    "bank_account_code_prefix",
    "cash_account_code_prefix",
    "transfer_account_code_prefix",
];
fn scope(id: i64) -> SettingsScope {
    SettingsScope::Company {
        company_id: WireId::try_from(id as u64).unwrap(),
    }
}
fn exact_error<T: std::fmt::Debug>(result: Result<T>, kind: ErrorKind, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

#[test]
fn company_configuration_installs_with_defaults_and_survives_upgrade() {
    big_stack(defaults_body);
}
async fn defaults_body() {
    let (db, orm, system) = install().await;
    let mut setup = orm.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    setup.install_audit_schema().await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    assert!(orm.check_audit_store().await.unwrap().allows_writes());
    let mut tx = orm
        .begin_opts(
            erp_sdk::BeginOptions::new(SUPERUSER_ID)
                .operation_identity(erp_sdk::OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let model = orm.registry().model("res.company").unwrap();
    assert!(model.check_company_auto);
    for field in ACCOUNT_FIELDS.into_iter().chain(JOURNAL_FIELDS) {
        assert!(model.field(field).unwrap().check_company.is_on(), "{field}");
    }
    let main = main_company(&mut tx).await;
    let second = company(&mut tx, main, "Independent company", None).await;
    for id in [main, second] {
        let c = records(&tx, id, "res.company").browse(vec![id]).unwrap();
        for field in ACCOUNT_FIELDS.into_iter().chain(JOURNAL_FIELDS) {
            assert_eq!(c.cell(&mut tx, field).await.unwrap(), Cell::Null, "{field}");
        }
        assert_eq!(
            c.cell(&mut tx, "tax_calculation_rounding_method")
                .await
                .unwrap(),
            text("round_globally")
        );
        assert_eq!(
            c.cell(&mut tx, "anglo_saxon_accounting").await.unwrap(),
            Cell::Bool(false)
        );
        assert_eq!(
            c.cell(&mut tx, "expects_chart_of_accounts").await.unwrap(),
            Cell::Bool(true)
        );
        assert_eq!(c.cell(&mut tx, "chart_template").await.unwrap(), Cell::Null);
    }
    let c = records(&tx, main, "res.company")
        .browse(vec![main])
        .unwrap();
    write(
        &c,
        &mut tx,
        RawVals::new().set_cell("bank_account_code_prefix", text("102")),
    )
    .await
    .unwrap();
    assert_eq!(
        records(&tx, second, "res.company")
            .browse(vec![second])
            .unwrap()
            .cell(&mut tx, "bank_account_code_prefix")
            .await
            .unwrap(),
        Cell::Null
    );
    write(
        &c,
        &mut tx,
        RawVals::new().set_cell(
            "tax_lock_date",
            Cell::Date(Date::from_ymd(2020, 1, 1).unwrap()),
        ),
    )
    .await
    .unwrap();
    let env = tx.env().with_company(main).unwrap();
    let read = read_values(
        &env,
        &mut tx,
        scope(main),
        vec!["account.fiscalyear_last_day".into()],
    )
    .await
    .unwrap();
    let revision = erp_sdk::serde_json::to_value(read).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    write_values(
        &env,
        &mut tx,
        scope(main),
        revision,
        vec![("account.fiscalyear_last_day".into(), Value::Int(30))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let env = orm.env(SUPERUSER_ID, Context::new());
    let upgraded =
        Box::pin(system.button_immediate_upgrade(&env, &[ModuleName::new("account").unwrap()]))
            .await
            .unwrap();
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    let c = records(&tx, main, "res.company")
        .browse(vec![main])
        .unwrap();
    assert_eq!(
        c.cell(&mut tx, "bank_account_code_prefix").await.unwrap(),
        text("102")
    );
    assert_eq!(
        c.cell(&mut tx, "account_price_include").await.unwrap(),
        text("tax_excluded")
    );
    for field in [
        "fiscalyear_lock_date",
        "sale_lock_date",
        "purchase_lock_date",
        "hard_lock_date",
        "stock_lock_date",
    ] {
        assert_eq!(c.cell(&mut tx, field).await.unwrap(), Cell::Null);
    }
    assert_eq!(
        c.cell(&mut tx, "tax_lock_date").await.unwrap(),
        Cell::Date(Date::from_ymd(2020, 1, 1).unwrap())
    );
    let env = tx.env().with_company(main).unwrap();
    let values = read_values(
        &env,
        &mut tx,
        scope(main),
        vec!["account.fiscalyear_last_day".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        erp_sdk::serde_json::to_value(values).unwrap()["values"][0]["value"],
        30
    );
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

#[test]
fn prefix_validation_and_chart_activation_refuse_atomically() {
    big_stack(validation_body);
}
async fn validation_body() {
    let (db, orm, _system) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let c = records(&tx, main, "res.company")
        .browse(vec![main])
        .unwrap();
    for field in PREFIX_FIELDS {
        for value in [" ", "10/2", "é", "10\n"] {
            exact_error(
                write(
                    &c,
                    &mut tx,
                    RawVals::new()
                        .set_cell(field, text(value))
                        .set_cell("anglo_saxon_accounting", Cell::Bool(true)),
                )
                .await,
                ErrorKind::Validation,
                &format!(
                    "{field} must be empty or contain only ASCII letters, digits, dots and dashes, up to 64 characters."
                ),
            );
            assert_eq!(
                c.cell(&mut tx, "anglo_saxon_accounting").await.unwrap(),
                Cell::Bool(false)
            );
        }
        let cap = "1".repeat(64);
        write(&c, &mut tx, RawVals::new().set_cell(field, text(&cap)))
            .await
            .unwrap();
        exact_error(
            write(
                &c,
                &mut tx,
                RawVals::new().set_cell(field, text(&"1".repeat(65))),
            )
            .await,
            ErrorKind::Validation,
            &format!(
                "{field} must be empty or contain only ASCII letters, digits, dots and dashes, up to 64 characters."
            ),
        );
        write(&c, &mut tx, RawVals::new().set_cell(field, text("")))
            .await
            .unwrap();
    }
    write(
        &c,
        &mut tx,
        RawVals::new().set_cell("tax_calculation_rounding_method", text("round_per_line")),
    )
    .await
    .unwrap();
    exact_error(
        write(
            &c,
            &mut tx,
            RawVals::new().set_cell("chart_template", text("custom")),
        )
        .await,
        ErrorKind::UserError,
        "Chart activation is unavailable until a later release.",
    );
    assert_eq!(c.cell(&mut tx, "chart_template").await.unwrap(), Cell::Null);
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

#[test]
fn company_references_are_checked_and_restricted() {
    big_stack(relations_body);
}
async fn relations_body() {
    let (db, orm, _system) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let branch = company(&mut tx, main, "Branch", Some(main)).await;
    let foreign = company(&mut tx, main, "Other root", None).await;
    let accounts = records(&tx, main, "account.account");
    let account = create(
        &accounts,
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("name", text("Configured account"))
                .set_cell("account_type", text("asset_current"))
                .set_cell("code", text("190000")),
        ],
    )
    .await
    .unwrap();
    let journals = records(&tx, main, "account.journal");
    let journal = create(
        &journals,
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("name", text("Adjustments"))
                .set_cell("type", text("general")),
        ],
    )
    .await
    .unwrap();
    for company_id in [main, branch] {
        let c = records(&tx, company_id, "res.company")
            .browse(vec![company_id])
            .unwrap();
        for field in ACCOUNT_FIELDS {
            write(
                &c,
                &mut tx,
                RawVals::new().set_cell(field, Cell::Id(account.ids()[0])),
            )
            .await
            .unwrap();
        }
        for field in JOURNAL_FIELDS {
            write(
                &c,
                &mut tx,
                RawVals::new().set_cell(field, Cell::Id(journal.ids()[0])),
            )
            .await
            .unwrap();
        }
    }
    let c = records(&tx, foreign, "res.company")
        .browse(vec![foreign])
        .unwrap();
    for (field, target) in ACCOUNT_FIELDS
        .into_iter()
        .map(|f| (f, account.ids()[0]))
        .chain(JOURNAL_FIELDS.into_iter().map(|f| (f, journal.ids()[0])))
    {
        let selected = c.clone();
        let error = tx
            .savepoint(async move |tx| {
                let error = selected
                    .write(
                        tx,
                        RawVals::new()
                            .set_cell(field, Cell::Id(target))
                            .set_cell("anglo_saxon_accounting", Cell::Bool(true)),
                    )
                    .await
                    .unwrap_err();
                // Check the inner rollback before the outer rollback clears the framework's
                // intentional failed-write poison for the next independent assertion.
                assert_eq!(
                    selected.cell(tx, "anglo_saxon_accounting").await.unwrap(),
                    Cell::Bool(false)
                );
                assert_eq!(selected.cell(tx, field).await.unwrap(), Cell::Null);
                Err::<(), _>(error)
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{field}: {error:?}");
        let label = &orm
            .registry()
            .model("res.company")
            .unwrap()
            .field(field)
            .unwrap()
            .string;
        let target_name = if ACCOUNT_FIELDS.contains(&field) {
            "Configured account"
        } else {
            "Adjustments"
        };
        assert_eq!(
            error.message(),
            format!(
                "These values link records of different companies:\n- The record is company “Other root”, and “{label}” ({field}: '{target_name}') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
            )
        );
        assert_eq!(c.cell(&mut tx, field).await.unwrap(), Cell::Null);
    }
    exact_error(
        write(
            &account,
            &mut tx,
            RawVals::new().set_commands(
                "company_ids",
                vec![Command::set([RecId::from_real(foreign).unwrap()])],
            ),
        )
        .await,
        ErrorKind::Validation,
        "An account used by company accounting configuration must remain available to that company.",
    );
    exact_error(
        write(
            &journal,
            &mut tx,
            RawVals::new().set_cell("company_id", Cell::Id(foreign)),
        )
        .await,
        ErrorKind::Validation,
        "A journal used by company accounting configuration must remain available to that company.",
    );
    let target = account.clone();
    let error = tx
        .savepoint(async move |tx| target.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'Company' (res.company)\nConstraint: 'Transfer account' (transfer_account_id)\nArchive the record instead of deleting it."
    );
    let target = journal.clone();
    let error = tx
        .savepoint(async move |tx| target.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'Company' (res.company)\nConstraint: 'Exchange difference journal' (currency_exchange_journal_id)\nArchive the record instead of deleting it."
    );
    for company_id in [main, branch] {
        let c = records(&tx, company_id, "res.company")
            .browse(vec![company_id])
            .unwrap();
        for field in ACCOUNT_FIELDS.into_iter().chain(JOURNAL_FIELDS) {
            write(&c, &mut tx, RawVals::new().set_cell(field, Cell::Null))
                .await
                .unwrap();
        }
    }
    account.unlink(&mut tx).await.unwrap();
    journal.unlink(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

#[test]
fn settings_links_to_selected_company_without_granting_company_writes() {
    big_stack(settings_body);
}
async fn settings_body() {
    let (db, orm, _system) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main = main_company(&mut tx).await;
    let second = company(&mut tx, main, "Second settings company", None).await;
    let unheld = company(&mut tx, main, "Unheld company", None).await;
    let hidden_accounts = records(&tx, unheld, "account.account");
    let hidden = create(
        &hidden_accounts,
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("name", text("Hidden account"))
                .set_cell("code", text("990000"))
                .set_cell("account_type", text("asset_current")),
        ],
    )
    .await
    .unwrap()
    .ids()[0];
    let system_group = group(&mut tx, "base", "group_system").await;
    let manager_group = group(&mut tx, "account", "group_account_manager").await;
    let admin = user_of(
        &mut tx,
        main,
        "company-admin",
        &[main, second],
        system_group,
    )
    .await;
    let manager = user_of(&mut tx, main, "company-accountant", &[main], manager_group).await;
    tx.commit().await.unwrap();
    drop(tx);
    let orm = policy_orm(&db, &orm).await;
    let mut tx = orm
        .begin(erp_sdk::UserId::new(admin).unwrap())
        .await
        .unwrap();
    let env = tx.env().with_company(second).unwrap();
    let unheld_record = env
        .records("res.company")
        .unwrap()
        .browse(vec![unheld])
        .unwrap();
    exact_error(
        write(
            &unheld_record,
            &mut tx,
            RawVals::new().set_cell("bank_account_code_prefix", text("NO")),
        )
        .await,
        ErrorKind::Access,
        "Choose a company available to your account.",
    );
    let selected = env
        .records("res.company")
        .unwrap()
        .browse(vec![second])
        .unwrap();
    for target in [hidden, i64::from(i32::MAX)] {
        exact_error(
            write(
                &selected,
                &mut tx,
                RawVals::new().set_cell("transfer_account_id", Cell::Id(target)),
            )
            .await,
            ErrorKind::Access,
            "The account or journal selected for transfer_account_id is unavailable.",
        );
    }
    let result = describe(&env, &mut tx, scope(second)).await.unwrap();
    let value = erp_sdk::serde_json::to_value(result).unwrap();
    let item = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "account.company_configuration")
        .expect("company Accounting Settings link is installed");
    assert_eq!(item["app"], "account");
    assert_eq!(item["block"], "Company accounting");
    assert_eq!(item["target"]["res_id"], second);
    let view_id=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='account' AND name='company_accounting_form'",&[]).await.unwrap()[0].get(0);
    let form = erp_addon_base::ir_ui_view::select::ViewType::new(&tx, "form").unwrap();
    let request = erp_addon_base::ir_ui_view::select::ViewRequest::new("res.company", &form)
        .view_id(erp_addon_base::ir_ui_view::select::ViewId::new(view_id).unwrap());
    let view = erp_addon_base::ir_ui_view::get_view(&mut tx, &env, &request)
        .await
        .unwrap();
    for field in ACCOUNT_FIELDS
        .into_iter()
        .chain(JOURNAL_FIELDS)
        .chain(PREFIX_FIELDS)
        .chain([
            "tax_calculation_rounding_method",
            "anglo_saxon_accounting",
            "expects_chart_of_accounts",
            "chart_template",
        ])
    {
        assert!(
            view.arch().contains(&format!("name=\"{field}\"")),
            "{field}"
        );
    }
    let c = env
        .records("res.company")
        .unwrap()
        .browse(vec![second])
        .unwrap();
    write(
        &c,
        &mut tx,
        RawVals::new().set_cell("bank_account_code_prefix", text("BANK")),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm
        .begin(erp_sdk::UserId::new(manager).unwrap())
        .await
        .unwrap();
    let env = tx.env();
    let c = env
        .records("res.company")
        .unwrap()
        .browse(vec![main])
        .unwrap();
    let error = write(
        &c,
        &mut tx,
        RawVals::new().set_cell("bank_account_code_prefix", text("NO")),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        "Your access rights do not let you modify 'Company' (res.company) records.\n\nGroups that may do this:\n\t- Settings Administration\n\nAsk an administrator if you need this access."
    );
    exact_error(
        describe(&env, &mut tx, scope(second)).await,
        ErrorKind::Access,
        "Choose a company available to your account.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let c = records(&tx, second, "res.company")
        .browse(vec![second])
        .unwrap();
    assert_eq!(
        c.cell(&mut tx, "bank_account_code_prefix").await.unwrap(),
        text("BANK")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

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

#[test]
fn installed_company_accounting_form_contract() {
    big_stack(browser_body);
}
async fn browser_body() {
    use std::io::Write;
    use std::process::{Command as ProcessCommand, Stdio};
    let root = addons_dir().parent().unwrap().to_owned();
    let assets = root.join("logs/company-native-assets");
    // Like native_wizard_installed's explicit browser extension, the HTTP contract always runs.
    // The author browser runner supplies a revision-bound request; absence is reported explicitly
    // and never skips the native install, metadata, write and persisted-read assertions below.
    let request = root.join("logs/company-browser.request");
    let browser_requested = request.try_exists().unwrap();
    if browser_requested {
        assert_eq!(
            std::fs::read_to_string(&request).unwrap().trim(),
            std::fs::read_to_string(root.join("REVISION"))
                .unwrap()
                .trim(),
            "browser request must name this source revision"
        );
    }
    let fixture = native_http::NativeFixture::start_with_assets(
        |_| {},
        browser_requested.then_some(assets.as_path()),
    )
    .await;
    let install = native_http::request_within(
        fixture.port,
        "POST",
        "/erp/module/install",
        &[
            ("cookie", &fixture.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        r#"{"modules":["account"]}"#,
        std::time::Duration::from_secs(120),
    );
    assert_eq!(install.status, 200, "account install: {}", install.body);
    let action: i64 = fixture.client.query_one("SELECT res_id::int8 FROM ir_model_data WHERE module='account' AND name='company_accounting_action'", &[]).await.unwrap().get(0);
    let company: i64 = fixture
        .client
        .query_one(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let created = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/account.account/create",
        r#"{"vals_list":[{"name":"Browser transfer account","code":"198777","account_type":"asset_current"}]}"#,
    );
    assert_eq!(created.status, 200, "account create: {}", created.body);
    let fields = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/res.company/fields_get",
        "{}",
    );
    assert_eq!(fields.status, 200, "company metadata: {}", fields.body);
    let fields: erp_sdk::serde_json::Value = erp_sdk::serde_json::from_str(&fields.body).unwrap();
    assert!(fields.get("transfer_account_id").is_some());
    assert!(fields.get("tax_calculation_rounding_method").is_some());
    let changed = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/res.company/write",
        &erp_sdk::serde_json::json!({"ids":[company],"vals":{"bank_account_code_prefix":"HTTP"}})
            .to_string(),
    );
    assert_eq!(changed.status, 200, "company write: {}", changed.body);
    let saved: String = fixture
        .client
        .query_one(
            "SELECT bank_account_code_prefix FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(saved, "HTTP");
    let view: i64 = fixture.client.query_one("SELECT res_id::int8 FROM ir_model_data WHERE module='account' AND name='company_accounting_form'", &[]).await.unwrap().get(0);
    let delivered = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/res.company/get_views",
        &erp_sdk::serde_json::json!({"wire_major":1,"views":[[view,"form"]],"options":{}})
            .to_string(),
    );
    assert_eq!(delivered.status, 200, "company form: {}", delivered.body);
    assert!(delivered.body.contains("transfer_account_id"));
    let status = if browser_requested {
        let mut child = ProcessCommand::new("bash")
        .current_dir(&root)
        .args(["-c", "set -euo pipefail; eval \"$(scripts/web-toolchain.sh env)\"; exec node web/scripts/browser-company-accounting.mjs"])
        .env(
            "ACCOUNT_COMPANY_OUTPUT",
            root.join(format!("logs/company-browser-{}", fixture.port)),
        )
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                erp_sdk::serde_json::json!({
                    "origin":format!("http://127.0.0.1:{}", fixture.port),
                    "cookies":fixture.cookies,"action":action,"company":company
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        Some(child.wait().unwrap())
    } else {
        eprintln!("Compiled browser walk not requested; native HTTP contract executed.");
        None
    };
    fixture.cleanup().await;
    if let Some(status) = status {
        assert!(
            status.success(),
            "installed company form browser acceptance failed"
        );
    }
}
