// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `purchase` addon on a full install: its default data, groups and access rows, and the
//! lifecycle of the order header as a purchase user and a purchase administrator, under the
//! installed access policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use erp_addon_base::ir_config_setting::{read_values, write_values};
use erp_addon_purchase::settings::{
    AMOUNT_DEFAULT, AMOUNT_SETTING, APPROVAL_SETTING, LOCK_SETTING, OrderPolicy,
};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_orm::{Command, Context, Orm, SUPERUSER_ID, UserId};
use erp_sdk::Decimal;
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::views::wire::{SettingsScope, WireId};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

#[path = "support/accounting.rs"]
mod bill_books;

#[path = "purchase_orders/remainder.rs"]
mod remainder;

#[path = "purchase_orders/bill_snapshot.rs"]
mod bill_snapshot;
#[path = "purchase_orders/billing.rs"]
mod billing;

#[path = "purchase_orders/replenish.rs"]
mod replenish;
#[path = "purchase_orders/returns.rs"]
mod returns;
#[path = "purchase_orders/settings.rs"]
mod settings;
#[path = "purchase_orders/warnings.rs"]
mod warnings;

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
fn run(body: impl std::future::Future<Output = ()> + Send + 'static) {
    run_body(move || body)
}

fn run_body<F: std::future::Future<Output = ()>>(build: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(build());
        })
        .unwrap()
        .join()
        .unwrap();
}

/// A fresh database with `purchase` installed, and an ORM that enforces the installed access data
/// as the server does.
async fn install(db: &TestDb) -> Orm {
    install_module(db, "purchase").await
}

fn purchase_module_system(db: &TestDb) -> ModuleSystem {
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

async fn install_module(db: &TestDb, module: &str) -> Orm {
    install_modules(db, &[module]).await
}

async fn install_modules(db: &TestDb, modules: &[&str]) -> Orm {
    let system = purchase_module_system(db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let installed = Box::pin(
        system.button_immediate_install(
            &env,
            &modules
                .iter()
                .map(|name| ModuleName::new(*name).unwrap())
                .collect::<Vec<_>>(),
        ),
    )
    .await
    .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    purchase_policy_orm(db, &installed.orm, &[]).await
}

async fn purchase_policy_orm(
    db: &TestDb,
    installed: &Orm,
    field_overrides: &[&'static erp_orm::FieldAccessImpl],
) -> Orm {
    let registry = Arc::clone(installed.registry());
    let compiler = erp_security::Compiler::new(Arc::clone(&registry))
        .field_overrides(field_overrides.iter().copied());
    let source = PolicySource::build_with(registry, db.pool().clone(), compiler)
        .await
        .unwrap();
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(Resolver::for_registry(installed.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

async fn strings(tx: &mut Transaction, sql: &str) -> Vec<String> {
    tx.query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect()
}

async fn xmlid(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module = $1 AND name = $2",
        &[&module, &name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

#[test]
fn purchase_installs_with_groups_access_sequence_and_settings() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        for name in ["purchase", "account", "analytic", "product"] {
            let states = strings(
                &mut tx,
                &format!("SELECT state::text FROM ir_module_module WHERE name = '{name}'"),
            )
            .await;
            assert_eq!(states, ["installed"], "state of {name}");
        }
        assert_eq!(
            strings(
                &mut tx,
                "SELECT d.name || '=' || g.name || '/' || coalesce(p.name, '-') \
                 FROM ir_model_data d JOIN res_groups g ON g.id = d.res_id \
                 LEFT JOIN res_groups_privilege p ON p.id = g.privilege_id \
                 WHERE d.module = 'purchase' AND d.model = 'res.groups' ORDER BY d.name"
            )
            .await,
            [
                "group_purchase_manager=Administrator/Purchase",
                "group_purchase_user=User/Purchase",
                "group_warning_purchase=Purchase warnings on products and vendors/-",
            ]
        );
        // group_warning_purchase is a light feature group and is not implied by base.group_user
        // by default.
        assert_eq!(
            strings(
                &mut tx,
                "SELECT count(*)::text FROM res_groups_implied_rel r \
                 JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = r.hid \
                 WHERE g.module = 'purchase' AND g.name = 'group_warning_purchase'"
            )
            .await,
            ["0"],
            "purchase.group_warning_purchase has no default implication"
        );
        assert_eq!(
            strings(
                &mut tx,
                "SELECT count(*)::text FROM res_groups_implied_rel r \
                 JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = r.gid \
                 JOIN ir_model_data l ON l.model = 'res.groups' AND l.res_id = r.hid \
                 WHERE g.module = 'purchase' AND g.name = 'group_warning_purchase' \
                 AND l.module = 'base' AND l.name = 'group_user_regular'"
            )
            .await,
            ["0"],
            "purchase.group_warning_purchase is classified as a light group"
        );
        // the administrator group implies the user group, the user group the internal user
        assert_eq!(
            strings(
                &mut tx,
                "SELECT h.name || '>' || l.module || '.' || l.name FROM res_groups_implied_rel r \
                 JOIN ir_model_data h ON h.model = 'res.groups' AND h.res_id = r.gid \
                 JOIN ir_model_data l ON l.model = 'res.groups' AND l.res_id = r.hid \
                 WHERE h.module = 'purchase' AND l.name <> 'group_user_regular' ORDER BY 1"
            )
            .await,
            [
                "group_purchase_manager>purchase.group_purchase_user",
                "group_purchase_user>base.group_user"
            ]
        );
        assert_eq!(
            strings(
                &mut tx,
                "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation \
                 || '/' || coalesce(a.generated, false)::text \
                 FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
                 LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
                 WHERE m.model IN ('purchase.order', 'purchase.order.line') ORDER BY 1"
            )
            .await
            .len(),
            6,
            "two permissions and the generated company restriction per model"
        );
        assert_eq!(
            strings(
                &mut tx,
                "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation \
                 FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
                 LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
                 WHERE m.model IN ('purchase.order', 'purchase.order.line') \
                 AND NOT coalesce(a.generated, false) ORDER BY 1"
            )
            .await,
            [
                "account.group_account_readonly/r",
                "account.group_account_readonly/r",
                "purchase.group_purchase_user/crud",
                "purchase.group_purchase_user/crud"
            ]
        );
        assert_eq!(
            strings(
                &mut tx,
                "SELECT s.code || '/' || s.prefix || '/' || s.padding::text || '/' \
                 || coalesce(s.company_id::text, 'shared') FROM ir_model_data d \
                 JOIN ir_sequence s ON s.id = d.res_id \
                 WHERE d.module = 'purchase' AND d.name = 'seq_purchase_order'"
            )
            .await,
            ["purchase.order/PO/5/shared"]
        );
        // the screens: two order actions and the settings page under the Purchase app
        assert_eq!(
            strings(
                &mut tx,
                "SELECT model || ':' || name FROM ir_model_data WHERE module = 'purchase' \
                 AND model IN ('ir.actions.act_window', 'ir.ui.menu') ORDER BY model, name COLLATE \"C\""
            )
            .await,
            [
                "ir.actions.act_window:action_product_supplierinfo",
                "ir.actions.act_window:action_purchase_order_analysis",
                "ir.actions.act_window:action_purchase_requisition",
                "ir.actions.act_window:action_purchase_vendors",
                "ir.actions.act_window:purchase_form_action",
                "ir.actions.act_window:purchase_rfq",
                "ir.actions.act_window:purchase_settings_action",
                "ir.ui.menu:menu_procurement_management",
                "ir.ui.menu:menu_purchase_analysis",
                "ir.ui.menu:menu_purchase_by_product_report",
                "ir.ui.menu:menu_purchase_config",
                "ir.ui.menu:menu_purchase_config_supplierinfo",
                "ir.ui.menu:menu_purchase_dynamic_report",
                "ir.ui.menu:menu_purchase_form_action",
                "ir.ui.menu:menu_purchase_general_settings",
                "ir.ui.menu:menu_purchase_product_template",
                "ir.ui.menu:menu_purchase_product_variant",
                "ir.ui.menu:menu_purchase_products",
                "ir.ui.menu:menu_purchase_report",
                "ir.ui.menu:menu_purchase_requisition",
                "ir.ui.menu:menu_purchase_rfq",
                "ir.ui.menu:menu_purchase_root",
                "ir.ui.menu:menu_purchase_supplierinfo",
                "ir.ui.menu:menu_purchase_vendors",
            ]
        );
        let orders = strings(&mut tx, "SELECT count(*)::text FROM purchase_order").await;
        assert_eq!(orders, ["0"], "the addon ships no order");
        let order_lines = strings(&mut tx, "SELECT count(*)::text FROM purchase_order_line").await;
        assert_eq!(order_lines, ["0"], "nor any order line");
        let main = xmlid(&mut tx, "base", "main_company").await;
        assert_eq!(
            OrderPolicy::read(&mut tx, main).await.unwrap(),
            OrderPolicy {
                two_step: false,
                amount: dec(&AMOUNT_DEFAULT.to_string()),
                lock: false
            }
        );
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

/// A user of `companies` (the first is the default) in `groups`.
async fn user(tx: &mut Transaction, login: &str, groups: &[i64], companies: &[i64]) -> UserId {
    let ids = |list: &[i64]| {
        list.iter()
            .map(|id| RecId::from_real(*id).unwrap())
            .collect()
    };
    let created = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(login))
                    .set_cell("login", text(login))
                    .set_cell("password", text(&format!("{login}-fixture-password")))
                    .set_cell("company_id", Cell::Id(companies[0]))
                    .set_commands("company_ids", [Command::Set(ids(companies))])
                    .set_commands("group_ids", [Command::Set(ids(groups))]),
            ],
        )
        .await
        .unwrap();
    UserId::new(created.ids()[0]).unwrap()
}

/// Write company-scope settings as the transaction's user (an administrator), with `company`
/// current, as the Settings page does.
async fn set(tx: &mut Transaction, company: i64, changes: Vec<(&str, Value)>) {
    let env = tx.env().with_company(company).unwrap();
    let scope = || SettingsScope::Company {
        company_id: WireId::try_from(u64::try_from(company).unwrap()).unwrap(),
    };
    let keys = changes.iter().map(|(k, _)| (*k).to_owned()).collect();
    let read = read_values(&env, tx, scope(), keys).await.unwrap();
    let revision = erp_sdk::serde_json::to_value(&read).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let changes = changes
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    write_values(&env, tx, scope(), revision, changes)
        .await
        .unwrap();
}

fn orders(tx: &Transaction, companies: &[i64]) -> Records {
    let active = Value::list(companies.iter().map(|c| Value::Int(*c)));
    tx.env()
        .with_context([("allowed_company_ids", active)])
        .records("purchase.order")
        .unwrap()
}

async fn order(tx: &mut Transaction, companies: &[i64], vals: RawVals) -> Records {
    let model = orders(tx, companies);
    model.create(tx, vec![vals]).await.unwrap()
}

/// Run `op` on `records` in a savepoint, so a refusal leaves the transaction usable.
async fn act(tx: &mut Transaction, records: &Records, op: &'static str) -> erp_sdk::Result<()> {
    let records = records.clone();
    tx.savepoint(async move |tx| records.call(tx, op, ()).await)
        .await
}

async fn write(tx: &mut Transaction, records: &Records, vals: RawVals) -> erp_sdk::Result<()> {
    let records = records.clone();
    tx.savepoint(async move |tx| {
        records.write(tx, vals).await?;
        tx.flush().await
    })
    .await
}

#[track_caller] // a failure names the case that was not refused
fn refused(result: erp_sdk::Result<impl std::fmt::Debug>, kind: ErrorKind, needles: &[&str]) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), kind, "{error:?}");
    for needle in needles {
        assert!(
            error.message().contains(needle),
            "expected {needle:?} in {:?}",
            error.message()
        );
    }
}

async fn written_at(tx: &mut Transaction, id: i64) -> Vec<String> {
    let sql = format!("SELECT write_date::text FROM purchase_order WHERE id = {id}");
    strings(tx, &sql).await
}

async fn cell(tx: &mut Transaction, records: &Records, field: &str) -> Cell {
    records.cell(tx, field).await.unwrap()
}

async fn state(tx: &mut Transaction, records: &Records) -> String {
    match cell(tx, records, "state").await {
        Cell::Str(state) => state.to_string(),
        other => panic!("state is {other:?}"),
    }
}

async fn name(tx: &mut Transaction, records: &Records) -> String {
    match cell(tx, records, "name").await {
        Cell::Str(name) => name.to_string(),
        other => panic!("name is {other:?}"),
    }
}

#[test]
fn order_lifecycle_as_purchase_user_and_administrator() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let employee = xmlid(&mut tx, "base", "group_user").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        // the base rules want an administrator before an ordinary user is created
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let manager = user(&mut tx, "purchase-manager", &[manager_group], &[main]).await;
        let clerk = user(&mut tx, "clerk", &[employee], &[main]).await;
        let vendor = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Vendor"))],
            )
            .await
            .unwrap()
            .ids()[0];
        let other_vendor = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Other vendor"))],
            )
            .await
            .unwrap()
            .ids()[0];
        let main_currency = match tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([main])
            .unwrap()
            .cell(&mut tx, "currency_id")
            .await
            .unwrap()
        {
            Cell::Id(id) => id,
            other => panic!("company currency {other:?}"),
        };
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        // an internal user without a purchase group sees no order
        let mut tx = orm.begin(clerk).await.unwrap();
        let error = orders(&tx, &[main])
            .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(buyer).await.unwrap();
        let by_vendor = || RawVals::new().set_cell("partner_id", Cell::Id(vendor));
        // defaults and numbering
        let first = order(&mut tx, &[main], by_vendor()).await;
        assert_eq!(state(&mut tx, &first).await, "draft");
        assert_eq!(name(&mut tx, &first).await, "PO00001");
        assert_eq!(cell(&mut tx, &first, "locked").await, Cell::Bool(false));
        assert_eq!(cell(&mut tx, &first, "date_approve").await, Cell::Null);
        assert_eq!(cell(&mut tx, &first, "company_id").await, Cell::Id(main));
        assert_eq!(
            cell(&mut tx, &first, "currency_id").await,
            Cell::Id(main_currency)
        );
        assert_eq!(
            cell(&mut tx, &first, "user_id").await,
            Cell::Id(buyer.get())
        );
        assert!(matches!(
            cell(&mut tx, &first, "date_order").await,
            Cell::DateTime(_)
        ));
        let second = order(&mut tx, &[main], by_vendor().set_cell("name", text("New"))).await;
        assert_eq!(name(&mut tx, &second).await, "PO00002");
        let imported = order(
            &mut tx,
            &[main],
            by_vendor().set_cell("name", text("IMPORT-7")),
        )
        .await;
        assert_eq!(name(&mut tx, &imported).await, "IMPORT-7");

        // operation fields and the number are never written by a client
        let model = orders(&tx, &[main]);
        let created = {
            let model = model.clone();
            let vals = by_vendor().set_cell("state", text("draft"));
            tx.savepoint(async move |tx| model.create(tx, vec![vals]).await)
                .await
        };
        refused(created, ErrorKind::Validation, &["state"]);
        for (field, value) in [
            ("state", text("purchase")),
            ("locked", Cell::Bool(true)),
            ("date_approve", Cell::Null),
            ("name", text("PO99999")),
        ] {
            let result = write(&mut tx, &first, RawVals::new().set_cell(field, value)).await;
            refused(result, ErrorKind::Validation, &[field]);
        }
        assert_eq!(state(&mut tx, &first).await, "draft");

        // nor through a context default, as the user or with sudo: no order is born confirmed
        for (field, value) in [
            ("state", Value::str("purchase")),
            ("locked", Value::Bool(true)),
            ("date_approve", Value::str("2026-01-01 00:00:00")),
            ("amount_total_cc", Value::Int(1)),
        ] {
            let key = format!("default_{field}");
            for su in [false, true] {
                let model = tx
                    .env()
                    .sudo(su)
                    .with_context([
                        (
                            "allowed_company_ids".to_owned(),
                            Value::list([Value::Int(main)]),
                        ),
                        (key.clone(), value.clone()),
                    ])
                    .records("purchase.order")
                    .unwrap();
                let vals = by_vendor();
                let created = tx
                    .savepoint(async move |tx| model.create(tx, vec![vals]).await)
                    .await;
                refused(created, ErrorKind::Validation, &[key.as_str()]);
            }
        }
        // a copy goes through the same create
        let copied = {
            let source = first.with_context([("default_state", Value::str("purchase"))]);
            tx.savepoint(async move |tx| source.copy(tx, None).await)
                .await
        };
        refused(copied, ErrorKind::Validation, &["default_state"]);
        let copied = {
            let source = first.clone();
            let mut defaults = erp_sdk::ValueMap::new();
            defaults.insert("state".into(), Value::str("purchase"));
            tx.savepoint(async move |tx| source.copy(tx, Some(defaults)).await)
                .await
        };
        refused(copied, ErrorKind::Validation, &["state"]);

        // confirm in one step; approve and reset are refused outside their source states
        act(&mut tx, &first, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &first).await, "purchase");
        assert!(matches!(
            cell(&mut tx, &first, "date_approve").await,
            Cell::DateTime(_)
        ));
        refused(
            act(&mut tx, &first, "button_approve").await,
            ErrorKind::Validation,
            &["PO00001", "purchase"],
        );
        refused(
            act(&mut tx, &first, "button_draft").await,
            ErrorKind::Validation,
            &["PO00001", "purchase"],
        );

        // a confirmed order keeps vendor, currency and company; other fields stay editable
        let result = write(
            &mut tx,
            &first,
            RawVals::new().set_cell("partner_id", Cell::Id(other_vendor)),
        )
        .await;
        refused(result, ErrorKind::Validation, &["PO00001", "partner_id"]);
        write(
            &mut tx,
            &first,
            RawVals::new().set_cell("origin", text("SO042")),
        )
        .await
        .unwrap();

        // lock: only a confirmed order; twice is no error; the buyer cannot unlock
        refused(
            act(&mut tx, &second, "button_lock").await,
            ErrorKind::Validation,
            &["PO00002", "draft"],
        );
        act(&mut tx, &first, "button_lock").await.unwrap();
        assert_eq!(cell(&mut tx, &first, "locked").await, Cell::Bool(true));
        act(&mut tx, &first, "button_lock").await.unwrap();
        refused(
            act(&mut tx, &first, "button_unlock").await,
            ErrorKind::Access,
            &["purchase.group_purchase_manager"],
        );
        refused(
            act(&mut tx, &first, "button_cancel").await,
            ErrorKind::UserError,
            &["PO00001"],
        );
        assert_eq!(cell(&mut tx, &first, "locked").await, Cell::Bool(true));

        // cancel: a cancelled order in the call is left as it is
        act(&mut tx, &second, "button_cancel").await.unwrap();
        assert_eq!(state(&mut tx, &second).await, "cancel");
        let before = written_at(&mut tx, second.ids()[0]).await;
        let both = model.browse([second.ids()[0], imported.ids()[0]]).unwrap();
        act(&mut tx, &both, "button_cancel").await.unwrap();
        assert_eq!(state(&mut tx, &imported).await, "cancel");
        assert_eq!(written_at(&mut tx, second.ids()[0]).await, before);

        // a cancelled order keeps every field; confirm refuses the whole call with it in
        let result = write(
            &mut tx,
            &second,
            RawVals::new().set_cell("origin", text("late")),
        )
        .await;
        refused(result, ErrorKind::Validation, &["PO00002", "origin"]);
        let third = order(&mut tx, &[main], by_vendor()).await;
        let mixed = model.browse([third.ids()[0], second.ids()[0]]).unwrap();
        refused(
            act(&mut tx, &mixed, "button_confirm").await,
            ErrorKind::Validation,
            &["PO00002", "cancel"],
        );
        assert_eq!(state(&mut tx, &third).await, "draft");

        // mark as sent changes only drafts; a sent order confirms
        let sent = model.browse([third.ids()[0], first.ids()[0]]).unwrap();
        act(&mut tx, &sent, "_mark_rfqs_as_sent").await.unwrap();
        assert_eq!(state(&mut tx, &third).await, "sent");
        assert_eq!(state(&mut tx, &first).await, "purchase");
        act(&mut tx, &third, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &third).await, "purchase");

        // delete only cancelled orders; back to draft from cancel
        let deleted = {
            let third = third.clone();
            tx.savepoint(async move |tx| third.unlink(tx).await).await
        };
        let third_name = name(&mut tx, &third).await;
        refused(deleted, ErrorKind::UserError, &[third_name.as_str()]);
        act(&mut tx, &imported, "button_draft").await.unwrap();
        assert_eq!(state(&mut tx, &imported).await, "draft");
        let gone = second.clone();
        tx.savepoint(async move |tx| gone.unlink(tx).await)
            .await
            .unwrap();

        // a copy is a new draft with a new number
        let copy = first.copy(&mut tx, None).await.unwrap();
        assert_eq!(state(&mut tx, &copy).await, "draft");
        assert_eq!(cell(&mut tx, &copy, "locked").await, Cell::Bool(false));
        assert_eq!(cell(&mut tx, &copy, "date_approve").await, Cell::Null);
        assert_eq!(cell(&mut tx, &copy, "origin").await, Cell::Null);
        assert_ne!(name(&mut tx, &copy).await, "PO00001");
        assert!(name(&mut tx, &copy).await.starts_with("PO"));
        let locked_id = first.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        // the administrator unlocks
        let mut tx = orm.begin(manager).await.unwrap();
        let first = orders(&tx, &[main]).browse([locked_id]).unwrap();
        act(&mut tx, &first, "button_unlock").await.unwrap();
        assert_eq!(cell(&mut tx, &first, "locked").await, Cell::Bool(false));
        act(&mut tx, &first, "button_unlock").await.unwrap();
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn each_order_follows_its_company_settings() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let currency: i64 = tx
            .query(
                "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                &[&main],
            )
            .await
            .unwrap()[0]
            .get(0);
        let other = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Second company"))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        let admin = user(&mut tx, "purchase-admin", &[system], &[main, other]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main, other]).await;
        let manager = user(
            &mut tx,
            "purchase-manager",
            &[manager_group],
            &[main, other],
        )
        .await;
        let vendor = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Vendor"))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            main,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(0.0)),
            ],
        )
        .await;
        set(&mut tx, other, vec![(LOCK_SETTING, Value::str("lock"))]).await;
        tx.commit().await.unwrap();
        drop(tx);

        let both = [main, other];
        let mut tx = orm.begin(buyer).await.unwrap();
        let vals = |company: i64| {
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("company_id", Cell::Id(company))
        };
        let in_main = order(&mut tx, &both, vals(main)).await;
        let in_other = order(&mut tx, &both, vals(other)).await;
        let call = orders(&tx, &both)
            .browse([in_main.ids()[0], in_other.ids()[0]])
            .unwrap();
        act(&mut tx, &call, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &in_main).await, "to approve");
        assert_eq!(cell(&mut tx, &in_main, "date_approve").await, Cell::Null);
        assert_eq!(state(&mut tx, &in_other).await, "purchase");
        assert_eq!(cell(&mut tx, &in_other, "locked").await, Cell::Bool(true));
        refused(
            act(&mut tx, &in_main, "button_approve").await,
            ErrorKind::Access,
            &["purchase.group_purchase_manager"],
        );
        assert_eq!(state(&mut tx, &in_main).await, "to approve");
        let waiting = in_main.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(manager).await.unwrap();
        let waiting = orders(&tx, &both).browse([waiting]).unwrap();
        act(&mut tx, &waiting, "button_approve").await.unwrap();
        assert_eq!(state(&mut tx, &waiting).await, "purchase");
        assert_eq!(cell(&mut tx, &waiting, "locked").await, Cell::Bool(false));
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

/// A number field of `records` (one record), normalized for comparison.
async fn num(tx: &mut Transaction, records: &Records, field: &str) -> Decimal {
    match cell(tx, records, field).await {
        Cell::Decimal(value) => value.normalize(),
        Cell::Float(value) => Decimal::try_from(value).unwrap().normalize(),
        Cell::Int(value) => Decimal::from(value),
        Cell::Null => Decimal::ZERO,
        other => panic!("{field} is {other:?}"),
    }
}

fn lines(tx: &Transaction, companies: &[i64]) -> Records {
    let active = Value::list(companies.iter().map(|c| Value::Int(*c)));
    tx.env()
        .with_context([("allowed_company_ids", active)])
        .records("purchase.order.line")
        .unwrap()
}

/// The lines of `order`, in their order.
async fn lines_of(tx: &mut Transaction, companies: &[i64], order: &Records) -> Vec<Records> {
    let found = lines(tx, companies)
        .search(
            tx,
            &Domain::condition("order_id", Operator::Eq, order.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    found
        .ids()
        .into_iter()
        .map(|id| found.browse([id]).unwrap())
        .collect()
}

/// Base 100 %, tax 100 % and tax -100 % for invoices and credit notes: a reverse charge.
fn reverse_charge() -> RawVals {
    let mut commands = vec![Command::Clear];
    for document in ["invoice", "refund"] {
        for (sequence, (kind, percent)) in [("base", "100"), ("tax", "100"), ("tax", "-100")]
            .into_iter()
            .enumerate()
        {
            commands.push(Command::Create(
                RawVals::new()
                    .set_cell("document_type", text(document))
                    .set_cell("repartition_type", text(kind))
                    .set_cell("factor_percent", Cell::Decimal(dec(percent)))
                    .set_cell("sequence", Cell::Int(i32::try_from(sequence).unwrap())),
            ));
        }
    }
    RawVals::new().set_commands("repartition_line_ids", commands)
}

/// The United States, the country the fixture taxes are made for (a tax needs one).
async fn us_country(tx: &mut Transaction) -> i64 {
    tx.query("SELECT id::int8 FROM res_country WHERE code = 'US'", &[])
        .await
        .unwrap()[0]
        .get(0)
}

fn line_vals(product: i64) -> RawVals {
    RawVals::new().set_cell("product_id", Cell::Id(product))
}

#[test]
fn order_lines_price_taxes_and_totals() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let dozen = xmlid(&mut tx, "uom", "product_uom_dozen").await;
        let us = us_country(&mut tx).await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let admin = user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let env = tx.env().with_company(main).unwrap();
        let create = async |tx: &mut Transaction, model: &str, vals: RawVals| -> i64 {
            env.records(model)
                .unwrap()
                .create(tx, vec![vals])
                .await
                .unwrap()
                .ids()[0]
        };
        let vendor = create(
            &mut tx,
            "res.partner",
            RawVals::new().set_cell("name", text("Vendor")),
        )
        .await;
        // a tax needs a tax group, and a plain install ships none
        let tax_group = create(
            &mut tx,
            "account.tax.group",
            RawVals::new().set_cell("name", text("Purchase taxes")),
        )
        .await;
        let tax = create(
            &mut tx,
            "account.tax",
            RawVals::new()
                .set_cell("name", text("Purchase 10%"))
                .set_cell("amount_type", text("percent"))
                .set_cell("amount", Cell::Decimal(dec("10")))
                .set_cell("type_tax_use", text("purchase"))
                .set_cell("company_id", Cell::Id(main))
                .set_cell("country_id", Cell::Id(us))
                .set_cell("tax_group_id", Cell::Id(tax_group)),
        )
        .await;
        let tax_id = RecId::from_real(tax).unwrap();
        let reverse = create(
            &mut tx,
            "account.tax",
            RawVals::new()
                .set_cell("name", text("Reverse 10%"))
                .set_cell("amount_type", text("percent"))
                .set_cell("amount", Cell::Decimal(dec("10")))
                .set_cell("type_tax_use", text("purchase"))
                .set_cell("company_id", Cell::Id(main))
                .set_cell("country_id", Cell::Id(us))
                .set_cell("tax_group_id", Cell::Id(tax_group)),
        )
        .await;
        env.records("account.tax")
            .unwrap()
            .browse([reverse])
            .unwrap()
            .write(&mut tx, reverse_charge())
            .await
            .unwrap();
        let bolt = create(
            &mut tx,
            "product.product",
            RawVals::new()
                .set_cell("name", text("Bolt"))
                .set_cell("standard_price", Cell::Decimal(dec("2.50")))
                .set_cell("uom_id", Cell::Id(unit))
                .set_commands("supplier_taxes_id", [Command::Set(vec![tax_id])]),
        )
        .await;
        let nut = create(
            &mut tx,
            "product.product",
            RawVals::new()
                .set_cell("name", text("Nut"))
                .set_cell("description_purchase", text("Hex, M8"))
                .set_cell("standard_price", Cell::Decimal(dec("9")))
                .set_cell("uom_id", Cell::Id(unit)),
        )
        .await;
        let bulk = create(
            &mut tx,
            "product.supplierinfo",
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("product_id", Cell::Id(nut))
                .set_cell("min_qty", Cell::Decimal(dec("10")))
                .set_cell("price", Cell::Decimal(dec("3")))
                .set_cell("discount", Cell::Decimal(dec("10")))
                .set_cell("delay", Cell::Int(5)),
        )
        .await;
        let single = create(
            &mut tx,
            "product.supplierinfo",
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("product_id", Cell::Id(nut))
                .set_cell("price", Cell::Decimal(dec("4")))
                .set_cell("delay", Cell::Int(1)),
        )
        .await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            main,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(50.0)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        // 011: the cost of the product, its unit, name and vendor taxes; amounts from the taxes
        let first = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_commands(
                    "order_line",
                    [Command::Create(
                        line_vals(bolt).set_cell("product_qty", Cell::Decimal(dec("4"))),
                    )],
                ),
        )
        .await;
        let deadline = cell(&mut tx, &first, "date_order").await;
        let bolt_line = lines_of(&mut tx, &c, &first).await.remove(0);
        assert_eq!(num(&mut tx, &bolt_line, "price_unit").await, dec("2.5"));
        assert_eq!(cell(&mut tx, &bolt_line, "uom_id").await, Cell::Id(unit));
        assert_eq!(cell(&mut tx, &bolt_line, "name").await, Cell::Null);
        assert_eq!(
            cell(&mut tx, &bolt_line, "tax_ids").await,
            Cell::Ids(vec![tax_id].into())
        );
        assert_eq!(cell(&mut tx, &bolt_line, "date_planned").await, deadline);
        assert_eq!(
            cell(&mut tx, &bolt_line, "selected_seller_id").await,
            Cell::Null
        );
        assert_eq!(num(&mut tx, &bolt_line, "price_subtotal").await, dec("10"));
        assert_eq!(num(&mut tx, &bolt_line, "price_tax").await, dec("1"));
        assert_eq!(num(&mut tx, &bolt_line, "price_total").await, dec("11"));
        assert_eq!(num(&mut tx, &first, "amount_untaxed").await, dec("10"));
        assert_eq!(num(&mut tx, &first, "amount_tax").await, dec("1"));
        assert_eq!(num(&mut tx, &first, "amount_total").await, dec("11"));
        assert_eq!(num(&mut tx, &first, "amount_total_cc").await, dec("11"));
        assert_eq!(cell(&mut tx, &first, "date_planned").await, deadline);

        // 012: the vendor price for the quantity, its discount and lead time; 013: another unit
        let order_id = first.ids()[0];
        let added = lines(&tx, &c)
            .create(
                &mut tx,
                vec![
                    line_vals(nut)
                        .set_cell("order_id", Cell::Id(order_id))
                        .set_cell("product_qty", Cell::Decimal(dec("12"))),
                    line_vals(bolt)
                        .set_cell("order_id", Cell::Id(order_id))
                        .set_cell("uom_id", Cell::Id(dozen)),
                ],
            )
            .await
            .unwrap();
        let nut_line = added.browse([added.ids()[0]]).unwrap();
        let dozen_line = added.browse([added.ids()[1]]).unwrap();
        assert_eq!(num(&mut tx, &nut_line, "price_unit").await, dec("3"));
        assert_eq!(num(&mut tx, &nut_line, "discount").await, dec("10"));
        assert_eq!(
            cell(&mut tx, &nut_line, "selected_seller_id").await,
            Cell::Id(bulk)
        );
        assert_eq!(cell(&mut tx, &nut_line, "name").await, text("Hex, M8"));
        let Cell::DateTime(at) = deadline else {
            panic!("{deadline:?}")
        };
        assert_eq!(
            cell(&mut tx, &nut_line, "date_planned").await,
            Cell::DateTime(DateTime::from_micros(at.micros() + 5 * 86_400_000_000))
        );
        assert_eq!(num(&mut tx, &nut_line, "price_subtotal").await, dec("32.4"));
        assert_eq!(num(&mut tx, &dozen_line, "price_unit").await, dec("30"));
        assert_eq!(
            num(&mut tx, &dozen_line, "product_uom_qty").await,
            dec("12")
        );
        assert_eq!(num(&mut tx, &first, "amount_untaxed").await, dec("72.4"));
        assert_eq!(num(&mut tx, &first, "amount_total").await, dec("76.4"));

        // 014: a smaller quantity takes the other vendor price; a price set by hand stays
        write(
            &mut tx,
            &nut_line,
            RawVals::new().set_cell("product_qty", Cell::Decimal(dec("2"))),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &nut_line, "price_unit").await, dec("4"));
        assert_eq!(num(&mut tx, &nut_line, "discount").await, dec("0"));
        assert_eq!(
            cell(&mut tx, &nut_line, "selected_seller_id").await,
            Cell::Id(single)
        );
        write(
            &mut tx,
            &nut_line,
            RawVals::new().set_cell("price_unit", Cell::Decimal(dec("1.5"))),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &nut_line,
            RawVals::new().set_cell("product_qty", Cell::Decimal(dec("20"))),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &nut_line, "price_unit").await, dec("1.5"));
        assert_eq!(num(&mut tx, &nut_line, "price_subtotal").await, dec("30"));

        // 015: amounts and the received quantity are not written by a client
        refused(
            write(
                &mut tx,
                &nut_line,
                RawVals::new().set_cell("price_subtotal", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::Validation,
            &["price_subtotal"],
        );
        refused(
            write(
                &mut tx,
                &nut_line,
                RawVals::new().set_cell("qty_received", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::UserError,
            &["not available yet"],
        );
        refused(
            write(
                &mut tx,
                &first,
                RawVals::new().set_cell("amount_total", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::Validation,
            &["amount_total"],
        );
        // nor through a context default, as the user or with sudo, nor on a line the order adds
        for field in [
            "price_subtotal",
            "selected_seller_id",
            "qty_received",
            "qty_received_method",
        ] {
            let key = format!("default_{field}");
            for su in [false, true] {
                let model = tx
                    .env()
                    .sudo(su)
                    .with_context([
                        (
                            "allowed_company_ids".to_owned(),
                            Value::list([Value::Int(main)]),
                        ),
                        (key.clone(), Value::Int(1)),
                    ])
                    .records("purchase.order.line")
                    .unwrap();
                let vals = line_vals(bolt).set_cell("order_id", Cell::Id(order_id));
                let created = tx
                    .savepoint(async move |tx| model.create(tx, vec![vals]).await)
                    .await;
                refused(created, ErrorKind::Validation, &[key.as_str()]);
            }
        }
        refused(
            write(
                &mut tx,
                &first.with_context([("default_qty_received", Value::Int(100))]),
                RawVals::new().set_commands("order_line", [Command::Create(line_vals(bolt))]),
            )
            .await,
            ErrorKind::Validation,
            &["default_qty_received"],
        );
        // a reverse-charge tax is refused by name, never computed as an ordinary tax
        let created = {
            let model = lines(&tx, &c);
            let vals = line_vals(bolt)
                .set_cell("order_id", Cell::Id(order_id))
                .set_commands(
                    "tax_ids",
                    [Command::Set(vec![RecId::from_real(reverse).unwrap()])],
                );
            tx.savepoint(async move |tx| model.create(tx, vec![vals]).await)
                .await
        };
        refused(
            created,
            ErrorKind::UserError,
            &["Reverse 10%", "reverse charge"],
        );
        assert_eq!(num(&mut tx, &first, "amount_untaxed").await, dec("70"));

        // 017: a line without a product stops the confirmation, naming the order
        let blank = lines(&tx, &c)
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("order_id", Cell::Id(order_id))
                        .set_cell("name", text("Freight")),
                ],
            )
            .await
            .unwrap();
        let first_name = name(&mut tx, &first).await;
        refused(
            act(&mut tx, &first, "button_confirm").await,
            ErrorKind::UserError,
            &[first_name.as_str()],
        );
        assert_eq!(state(&mut tx, &first).await, "draft");
        blank.unlink(&mut tx).await.unwrap();

        // 018: the total in the company currency meets the minimum amount: a manager approves
        act(&mut tx, &first, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &first).await, "to approve");
        let small = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_commands("order_line", [Command::Create(line_vals(bolt))]),
        )
        .await;
        act(&mut tx, &small, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &small).await, "purchase");

        // 016: lines of a confirmed order are added, not deleted; a cancelled order takes none
        let small_line = lines_of(&mut tx, &c, &small).await.remove(0);
        refused(
            {
                let line = small_line.clone();
                tx.savepoint(async move |tx| line.unlink(tx).await).await
            },
            ErrorKind::UserError,
            &["delete lines of"],
        );
        lines(&tx, &c)
            .create(
                &mut tx,
                vec![line_vals(bolt).set_cell("order_id", Cell::Id(small.ids()[0]))],
            )
            .await
            .unwrap();
        assert_eq!(num(&mut tx, &small, "amount_untaxed").await, dec("5"));
        act(&mut tx, &small, "button_lock").await.unwrap();
        refused(
            write(
                &mut tx,
                &small_line,
                RawVals::new().set_cell("product_qty", Cell::Decimal(dec("3"))),
            )
            .await,
            ErrorKind::UserError,
            &["locked"],
        );
        refused(
            write(
                &mut tx,
                &small,
                RawVals::new().set_commands("order_line", [Command::Create(line_vals(bolt))]),
            )
            .await,
            ErrorKind::Validation,
            &["order_line"],
        );

        // 019: a copy carries the lines and their prices, with planned dates from its own deadline
        let copied = small.copy(&mut tx, None).await.unwrap();
        let copied_lines = lines_of(&mut tx, &c, &copied).await;
        assert_eq!(copied_lines.len(), 2);
        assert_eq!(num(&mut tx, &copied, "amount_untaxed").await, dec("5"));
        let copy_deadline = cell(&mut tx, &copied, "date_order").await;
        assert_eq!(
            cell(&mut tx, &copied_lines[0], "date_planned").await,
            copy_deadline
        );
        assert_eq!(state(&mut tx, &copied).await, "draft");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// Run `create` on `model` in a savepoint, so a refusal leaves the transaction usable.
async fn try_create(
    tx: &mut Transaction,
    model: &Records,
    vals: RawVals,
) -> erp_sdk::Result<Records> {
    let model = model.clone();
    tx.savepoint(async move |tx| {
        let created = model.create(tx, vec![vals]).await?;
        tx.flush().await?;
        Ok(created)
    })
    .await
}

/// Vendor, a 10 % purchase tax, a bolt bought at its cost with that tax and a nut with a purchase
/// description and two vendor prices (3 with 10 % off from 10 units and 5 days, 4 from one).
struct Catalog {
    vendor: i64,
    tax: RecId,
    bolt: i64,
    nut: i64,
    unit: i64,
    dozen: i64,
}

async fn catalog(tx: &mut Transaction, main: i64) -> Catalog {
    let env = tx.env().with_company(main).unwrap();
    let create = async |tx: &mut Transaction, model: &str, vals: RawVals| -> i64 {
        env.records(model)
            .unwrap()
            .create(tx, vec![vals])
            .await
            .unwrap()
            .ids()[0]
    };
    let unit = xmlid(tx, "uom", "product_uom_unit").await;
    let dozen = xmlid(tx, "uom", "product_uom_dozen").await;
    let us = us_country(tx).await;
    let tax_group = create(
        tx,
        "account.tax.group",
        RawVals::new().set_cell("name", text("Purchase taxes")),
    )
    .await;
    let vendor = create(
        tx,
        "res.partner",
        RawVals::new().set_cell("name", text("Vendor")),
    )
    .await;
    let tax = create(
        tx,
        "account.tax",
        RawVals::new()
            .set_cell("name", text("Purchase 10%"))
            .set_cell("amount_type", text("percent"))
            .set_cell("amount", Cell::Decimal(dec("10")))
            .set_cell("type_tax_use", text("purchase"))
            .set_cell("company_id", Cell::Id(main))
            .set_cell("country_id", Cell::Id(us))
            .set_cell("tax_group_id", Cell::Id(tax_group)),
    )
    .await;
    let tax = RecId::from_real(tax).unwrap();
    let bolt = create(
        tx,
        "product.product",
        RawVals::new()
            .set_cell("name", text("Bolt"))
            .set_cell("standard_price", Cell::Decimal(dec("2.50")))
            .set_cell("uom_id", Cell::Id(unit))
            .set_commands("supplier_taxes_id", [Command::Set(vec![tax])]),
    )
    .await;
    let nut = create(
        tx,
        "product.product",
        RawVals::new()
            .set_cell("name", text("Nut"))
            .set_cell("description_purchase", text("Hex, M8"))
            .set_cell("standard_price", Cell::Decimal(dec("9")))
            .set_cell("uom_id", Cell::Id(unit)),
    )
    .await;
    for (min_qty, price, discount, delay) in [("10", "3", "10", 5), ("0", "4", "0", 1)] {
        create(
            tx,
            "product.supplierinfo",
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("product_id", Cell::Id(nut))
                .set_cell("min_qty", Cell::Decimal(dec(min_qty)))
                .set_cell("price", Cell::Decimal(dec(price)))
                .set_cell("discount", Cell::Decimal(dec(discount)))
                .set_cell("delay", Cell::Int(delay)),
        )
        .await;
    }
    Catalog {
        vendor,
        tax,
        bolt,
        nut,
        unit,
        dozen,
    }
}

/// What a line shows: unit, description, taxes, price, discount, subtotal and tax.
async fn shown(
    tx: &mut Transaction,
    line: &Records,
) -> (Cell, Cell, Cell, Decimal, Decimal, Decimal, Decimal) {
    (
        cell(tx, line, "uom_id").await,
        cell(tx, line, "name").await,
        cell(tx, line, "tax_ids").await,
        num(tx, line, "price_unit").await,
        num(tx, line, "discount").await,
        num(tx, line, "price_subtotal").await,
        num(tx, line, "price_tax").await,
    )
}

#[test]
fn lines_through_order_commands_and_a_copy() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        // what the line form sends: its default price, discount and technical price, untouched or typed
        let form_line = |product: i64, price: &str| {
            line_vals(product)
                .set_cell("price_unit", Cell::Decimal(dec(price)))
                .set_cell("technical_price_unit", Cell::Decimal(dec("0")))
                .set_cell("discount", Cell::Decimal(dec("0")))
        };
        let po = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [
                        Command::Create(
                            line_vals(k.nut).set_cell("product_qty", Cell::Decimal(dec("12"))),
                        ),
                        Command::Create(line_vals(k.bolt).set_cell("uom_id", Cell::Id(k.dozen))),
                        Command::Create(
                            form_line(k.nut, "0").set_cell("product_qty", Cell::Decimal(dec("12"))),
                        ),
                        Command::Create(form_line(k.bolt, "7")),
                    ],
                ),
        )
        .await;
        let made = lines_of(&mut tx, &c, &po).await;
        assert_eq!(made.len(), 4);
        let taxed = Cell::Ids(vec![k.tax].into());
        let untaxed = Cell::Ids(Vec::new().into());
        let expected = [
            (
                Cell::Id(k.unit),
                text("Hex, M8"),
                untaxed.clone(),
                "3",
                "10",
                "32.4",
                "0",
            ),
            (
                Cell::Id(k.dozen),
                Cell::Null,
                taxed.clone(),
                "30",
                "0",
                "30",
                "3",
            ),
            (
                Cell::Id(k.unit),
                text("Hex, M8"),
                untaxed.clone(),
                "3",
                "10",
                "32.4",
                "0",
            ),
            (
                Cell::Id(k.unit),
                Cell::Null,
                taxed.clone(),
                "7",
                "0",
                "7",
                "0.7",
            ),
        ];
        for (line, (unit, name, taxes, price, discount, subtotal, tax)) in
            made.iter().zip(&expected)
        {
            assert_eq!(
                shown(&mut tx, line).await,
                (
                    unit.clone(),
                    name.clone(),
                    taxes.clone(),
                    dec(price).normalize(),
                    dec(discount).normalize(),
                    dec(subtotal).normalize(),
                    dec(tax).normalize(),
                ),
                "line {:?}",
                line.ids()
            );
        }
        assert_eq!(num(&mut tx, &po, "amount_untaxed").await, dec("101.8"));
        assert_eq!(num(&mut tx, &po, "amount_tax").await, dec("3.7"));
        assert_eq!(num(&mut tx, &po, "amount_total").await, dec("105.5"));

        let copy = po.copy(&mut tx, None).await.unwrap();
        assert_eq!(state(&mut tx, &copy).await, "draft");
        assert_ne!(name(&mut tx, &copy).await, name(&mut tx, &po).await);
        let copied = lines_of(&mut tx, &c, &copy).await;
        assert_eq!(copied.len(), 4);
        for (original, line) in made.iter().zip(&copied) {
            assert_eq!(shown(&mut tx, line).await, shown(&mut tx, original).await);
        }
        for field in ["amount_untaxed", "amount_tax", "amount_total"] {
            assert_eq!(
                num(&mut tx, &copy, field).await,
                num(&mut tx, &po, field).await,
                "{field}"
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn state_amounts_and_received_quantity_refused_by_every_path() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        let header = || RawVals::new().set_cell("partner_id", Cell::Id(k.vendor));
        let with_line =
            |line: RawVals| header().set_commands("order_line", [Command::Create(line)]);
        // context defaults of the order create, also those that would reach its lines
        for (key, value) in [
            ("default_state", Value::str("purchase")),
            ("default_locked", Value::Bool(true)),
            ("default_date_approve", Value::str("2026-01-01 00:00:00")),
            ("default_amount_total", Value::Int(1)),
            ("default_qty_received", Value::Int(1)),
            ("default_price_subtotal", Value::Int(1)),
        ] {
            let model = orders(&tx, &c).with_context([(key, value)]);
            refused(
                try_create(&mut tx, &model, with_line(line_vals(k.bolt))).await,
                ErrorKind::Validation,
                &[key],
            );
        }
        let orders_of_c = orders(&tx, &c);
        let po = order(&mut tx, &c, with_line(line_vals(k.bolt))).await;
        let line = lines_of(&mut tx, &c, &po).await.remove(0);
        let line_model = lines(&tx, &c).with_context([("default_price_total", Value::Int(1))]);
        refused(
            try_create(
                &mut tx,
                &line_model,
                line_vals(k.bolt).set_cell("order_id", Cell::Id(po.ids()[0])),
            )
            .await,
            ErrorKind::Validation,
            &["default_price_total"],
        );
        // values inside the order's order_line commands, at create and at write
        refused(
            try_create(
                &mut tx,
                &orders_of_c,
                with_line(line_vals(k.bolt).set_cell("price_subtotal", Cell::Decimal(dec("1")))),
            )
            .await,
            ErrorKind::Validation,
            &["price_subtotal"],
        );
        refused(
            try_create(
                &mut tx,
                &orders_of_c,
                with_line(line_vals(k.bolt).set_cell("qty_received", Cell::Decimal(dec("1")))),
            )
            .await,
            ErrorKind::UserError,
            &["qty_received"],
        );
        let line_id = RecId::from_real(line.ids()[0]).unwrap();
        for (field, kind) in [
            ("price_total", ErrorKind::Validation),
            ("product_uom_qty", ErrorKind::Validation),
            ("qty_received", ErrorKind::UserError),
        ] {
            refused(
                write(
                    &mut tx,
                    &po,
                    RawVals::new().set_commands(
                        "order_line",
                        [Command::Update(
                            line_id,
                            RawVals::new().set_cell(field, Cell::Decimal(dec("1"))),
                        )],
                    ),
                )
                .await,
                kind,
                &[field],
            );
        }
        // the order's state through the line's related field, and the number on the order
        refused(
            write(
                &mut tx,
                &line,
                RawVals::new().set_cell("state", text("purchase")),
            )
            .await,
            ErrorKind::Validation,
            &["state"],
        );
        refused(
            write(
                &mut tx,
                &po,
                RawVals::new().set_cell("name", text("PO-HAND")),
            )
            .await,
            ErrorKind::Validation,
            &["name"],
        );
        assert_eq!(state(&mut tx, &po).await, "draft");
        assert_eq!(num(&mut tx, &line, "qty_received").await, dec("0"));
        assert_eq!(num(&mut tx, &po, "amount_untaxed").await, dec("2.5"));
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn purchase_access_for_users_of_two_companies() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let currency: i64 = tx
            .query(
                "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                &[&main],
            )
            .await
            .unwrap()[0]
            .get(0);
        let other = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Second company"))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let system = xmlid(&mut tx, "base", "group_system").await;
        let employee = xmlid(&mut tx, "base", "group_user").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        user(&mut tx, "purchase-admin", &[system], &[main, other]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let manager = user(
            &mut tx,
            "purchase-manager",
            &[manager_group],
            &[other, main],
        )
        .await;
        let clerk = user(&mut tx, "clerk", &[employee], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let both = [main, other];
        let by_vendor = |company: i64| {
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_cell("company_id", Cell::Id(company))
                .set_commands("order_line", [Command::Create(line_vals(k.bolt))])
        };
        // the administrator of both companies orders in each and locks the second
        let mut tx = orm.begin(manager).await.unwrap();
        let in_other = order(&mut tx, &both, by_vendor(other)).await;
        act(&mut tx, &in_other, "button_confirm").await.unwrap();
        act(&mut tx, &in_other, "button_lock").await.unwrap();
        let other_id = in_other.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        // the purchase user of the first company orders there and sees nothing of the second
        let mut tx = orm.begin(buyer).await.unwrap();
        let orders_of_main = orders(&tx, &[main]);
        let mine = order(&mut tx, &[main], by_vendor(main)).await;
        act(&mut tx, &mine, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &mine).await, "purchase");
        let all = Domain::constant(true);
        let seen = orders(&tx, &[main]).search(&mut tx, &all).await.unwrap();
        assert_eq!(seen.ids(), mine.ids());
        let seen_lines = lines(&tx, &[main]).search(&mut tx, &all).await.unwrap();
        assert_eq!(seen_lines.len(), 1);
        assert!(
            try_create(&mut tx, &orders_of_main, by_vendor(other))
                .await
                .is_err(),
            "an order in a company the user is not in"
        );
        let foreign = orders(&tx, &[main]).browse([other_id]).unwrap();
        assert!(
            act(&mut tx, &foreign, "button_cancel").await.is_err(),
            "an order of the other company"
        );
        let mine_id = mine.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        // a user of neither group creates and reads no order
        let mut tx = orm.begin(clerk).await.unwrap();
        let orders_of_main = orders(&tx, &[main]);
        refused(
            try_create(&mut tx, &orders_of_main, by_vendor(main)).await,
            ErrorKind::Access,
            &[],
        );
        refused(
            orders(&tx, &[main])
                .search_count(&mut tx, &Domain::constant(true), None)
                .await,
            ErrorKind::Access,
            &[],
        );
        tx.rollback().await.unwrap();
        drop(tx);

        // the administrator sees both orders and unlocks the locked one
        let mut tx = orm.begin(manager).await.unwrap();
        let seen = orders(&tx, &both)
            .search(&mut tx, &Domain::constant(true))
            .await
            .unwrap();
        let mut ids = seen.ids();
        ids.sort_unstable();
        let mut expected = vec![mine_id, other_id];
        expected.sort_unstable();
        assert_eq!(ids, expected);
        let locked = orders(&tx, &both).browse([other_id]).unwrap();
        act(&mut tx, &locked, "button_unlock").await.unwrap();
        assert_eq!(cell(&mut tx, &locked, "locked").await, Cell::Bool(false));
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_company_change_converts_the_approval_total() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let usd = xmlid(&mut tx, "base", "USD").await;
        let eur = xmlid(&mut tx, "base", "EUR").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let env = tx.env().clone();
        env.records("res.currency")
            .unwrap()
            .browse([eur])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await
            .unwrap();
        let create = async |tx: &mut Transaction, env: &Env, model: &str, vals: RawVals| -> i64 {
            env.records(model)
                .unwrap()
                .create(tx, vec![vals])
                .await
                .unwrap()
                .ids()[0]
        };
        let euro = create(
            &mut tx,
            &env,
            "res.company",
            RawVals::new()
                .set_cell("name", text("Euro company"))
                .set_cell("currency_id", Cell::Id(eur)),
        )
        .await;
        // one dollar buys ten euros in the euro company
        for (currency, rate) in [(eur, "1"), (usd, "0.1")] {
            create(
                &mut tx,
                &env.with_company(euro).unwrap(),
                "res.currency.rate",
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(currency))
                    .set_cell("company_id", Cell::Id(euro))
                    .set_cell("name", Cell::Date(Date::from_ymd(2020, 1, 1).unwrap()))
                    .set_cell("rate", Cell::Decimal(dec(rate))),
            )
            .await;
        }
        // older rates; a rate counts from the day after its date, so 31 May 2019 is told apart from 1 June
        for (currency, rate, year, month, day) in [
            (eur, "1", 2018, 1, 1),
            (usd, "0.5", 2018, 1, 1),
            (usd, "0.25", 2019, 5, 31),
        ] {
            create(
                &mut tx,
                &env.with_company(euro).unwrap(),
                "res.currency.rate",
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(currency))
                    .set_cell("company_id", Cell::Id(euro))
                    .set_cell(
                        "name",
                        Cell::Date(Date::from_ymd(year, month, day).unwrap()),
                    )
                    .set_cell("rate", Cell::Decimal(dec(rate))),
            )
            .await;
        }
        let admin = user(&mut tx, "purchase-admin", &[system], &[main, euro]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main, euro]).await;
        let vendor = create(
            &mut tx,
            &env,
            "res.partner",
            RawVals::new().set_cell("name", text("Vendor")),
        )
        .await;
        let crate_product = create(
            &mut tx,
            &env.with_company(main).unwrap(),
            "product.product",
            RawVals::new()
                .set_cell("name", text("Crate"))
                .set_cell("standard_price", Cell::Decimal(dec("11")))
                .set_cell("uom_id", Cell::Id(unit)),
        )
        .await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            euro,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(50.0)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);

        let both = [main, euro];
        let mut tx = orm.begin(buyer).await.unwrap();
        let draft = order(
            &mut tx,
            &both,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("company_id", Cell::Id(main))
                .set_commands("order_line", [Command::Create(line_vals(crate_product))]),
        )
        .await;
        assert_eq!(num(&mut tx, &draft, "amount_total_cc").await, dec("11"));
        // a draft with lines keeps its company: their taxes and prices belong to it
        refused(
            write(
                &mut tx,
                &draft,
                RawVals::new().set_cell("company_id", Cell::Id(euro)),
            )
            .await,
            ErrorKind::Validation,
            &["has lines"],
        );
        // 11 dollars under the dollar company's rules; 110 euros, above the euro company's 50
        let old_line = lines_of(&mut tx, &both, &draft).await.remove(0);
        old_line.unlink(&mut tx).await.unwrap();
        write(
            &mut tx,
            &draft,
            RawVals::new().set_cell("company_id", Cell::Id(euro)),
        )
        .await
        .unwrap();
        lines(&tx, &both)
            .create(
                &mut tx,
                vec![
                    line_vals(crate_product)
                        .set_cell("order_id", Cell::Id(draft.ids()[0]))
                        .set_cell("price_unit", Cell::Decimal(dec("11"))),
                ],
            )
            .await
            .unwrap();
        assert_eq!(num(&mut tx, &draft, "amount_total").await, dec("11"));
        assert_eq!(num(&mut tx, &draft, "amount_total_cc").await, dec("110"));
        act(&mut tx, &draft, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &draft).await, "to approve");
        // the stored total converts at the UTC day of the deadline, whoever saves it: 20:00 UTC on
        // 31 May 2019 is 1 June in Jakarta, but the rate dated 31 May does not count before 31 May
        // itself, so every reader gets two euros to the dollar (22), not four (44)
        let late_may = DateTime::parse_odoo("2019-05-31 20:00:00").unwrap();
        for zone in [Some("Asia/Jakarta"), None] {
            let (model_orders, model_lines) = match zone {
                Some(zone) => (
                    orders(&tx, &both).with_context([("tz", Value::str(zone))]),
                    lines(&tx, &both).with_context([("tz", Value::str(zone))]),
                ),
                None => (orders(&tx, &both), lines(&tx, &both)),
            };
            let header = || {
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("company_id", Cell::Id(euro))
                    .set_cell("currency_id", Cell::Id(usd))
                    .set_cell("date_order", Cell::DateTime(late_may))
            };
            let line = || line_vals(crate_product).set_cell("price_unit", Cell::Decimal(dec("11")));
            // lines created apart from the order
            let apart = model_orders.create(&mut tx, vec![header()]).await.unwrap();
            model_lines
                .create(
                    &mut tx,
                    vec![line().set_cell("order_id", Cell::Id(apart.ids()[0]))],
                )
                .await
                .unwrap();
            assert_eq!(num(&mut tx, &apart, "amount_total_cc").await, dec("22"));
            // lines created with the order, the way the form saves it
            let one_call = model_orders
                .create(
                    &mut tx,
                    vec![header().set_commands("order_line", [Command::Create(line())])],
                )
                .await
                .unwrap();
            assert_eq!(num(&mut tx, &one_call, "amount_total_cc").await, dec("22"));
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// Run `unlink` on `records` in a savepoint, so a refusal leaves the transaction usable.
async fn try_unlink(tx: &mut Transaction, records: &Records) -> erp_sdk::Result<()> {
    let records = records.clone();
    tx.savepoint(async move |tx| records.unlink(tx).await).await
}

#[test]
fn a_caller_without_the_right_is_refused_before_an_order_is_read() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let employee = xmlid(&mut tx, "base", "group_user").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let clerk = user(&mut tx, "clerk", &[employee], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let header = || RawVals::new().set_cell("partner_id", Cell::Id(k.vendor));
        let mut tx = orm.begin(buyer).await.unwrap();
        let cancelled = order(&mut tx, &c, header()).await;
        act(&mut tx, &cancelled, "button_cancel").await.unwrap();
        let draft = order(&mut tx, &c, header()).await;
        let names = [name(&mut tx, &cancelled).await, name(&mut tx, &draft).await];
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let model = orders(&tx, &c);
        let cancelled = model.browse(cancelled.ids()).unwrap();
        let draft = model.browse(draft.ids()).unwrap();
        // each refusal is the access refusal and shows no number or state of the order
        let mut errors = vec![
            write(
                &mut tx,
                &cancelled,
                RawVals::new().set_cell("origin", text("x")),
            )
            .await
            .unwrap_err(),
            try_unlink(&mut tx, &draft).await.unwrap_err(),
            try_create(&mut tx, &model, header()).await.unwrap_err(),
        ];
        errors.push(
            write(
                &mut tx,
                &draft,
                RawVals::new().set_cell("origin", text("x")),
            )
            .await
            .unwrap_err(),
        );
        for error in &errors {
            assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
            for number in &names {
                assert!(!error.message().contains(number), "{error:?}");
            }
            assert!(!error.message().contains("cancel"), "{error:?}");
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_line_never_writes_the_copies_of_its_order() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        let po = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands("order_line", [Command::Create(line_vals(k.bolt))]),
        )
        .await;
        let line = lines_of(&mut tx, &c, &po).await.remove(0);
        for (field, value) in [
            ("company_id", Cell::Id(main)),
            ("partner_id", Cell::Id(k.vendor)),
            ("technical_price_unit", Cell::Decimal(dec("1"))),
        ] {
            refused(
                write(
                    &mut tx,
                    &line,
                    RawVals::new().set_cell(field, value.clone()),
                )
                .await,
                ErrorKind::Validation,
                &[field],
            );
            refused(
                write(
                    &mut tx,
                    &po,
                    RawVals::new().set_commands(
                        "order_line",
                        [Command::Update(
                            RecId::from_real(line.ids()[0]).unwrap(),
                            RawVals::new().set_cell(field, value),
                        )],
                    ),
                )
                .await,
                ErrorKind::Validation,
                &[field],
            );
        }
        let order_id = Cell::Id(po.ids()[0]);
        for field in ["company_id", "partner_id"] {
            let value = Cell::Id(if field == "company_id" {
                main
            } else {
                k.vendor
            });
            let plain = lines(&tx, &c);
            refused(
                try_create(
                    &mut tx,
                    &plain,
                    line_vals(k.bolt)
                        .set_cell("order_id", order_id.clone())
                        .set_cell(field, value),
                )
                .await,
                ErrorKind::Validation,
                &[field],
            );
            let model = if field == "company_id" {
                lines(&tx, &c).with_context([("default_company_id", Value::Int(main))])
            } else {
                lines(&tx, &c).with_context([("default_partner_id", Value::Int(k.vendor))])
            };
            refused(
                try_create(
                    &mut tx,
                    &model,
                    line_vals(k.bolt).set_cell("order_id", order_id.clone()),
                )
                .await,
                ErrorKind::Validation,
                &[field],
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn lines_of_an_approved_order_keep_the_approval_limit() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        let admin = user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let manager = user(&mut tx, "purchase-manager", &[manager_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            main,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(100.0)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let header = || RawVals::new().set_cell("partner_id", Cell::Id(k.vendor));
        // an approved order of one bolt (2.75 with tax), under the limit
        let mut tx = orm.begin(manager).await.unwrap();
        let approved = order(
            &mut tx,
            &c,
            header().set_commands("order_line", [Command::Create(line_vals(k.bolt))]),
        )
        .await;
        act(&mut tx, &approved, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &approved).await, "purchase");
        let approved_id = approved.ids()[0];
        let approved_name = name(&mut tx, &approved).await;
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(buyer).await.unwrap();
        let approved = orders(&tx, &c).browse([approved_id]).unwrap();
        let line = lines_of(&mut tx, &c, &approved).await.remove(0);
        let quantity =
            |value: &str| RawVals::new().set_cell("product_qty", Cell::Decimal(dec(value)));
        // a raise past the limit (1000 bolts, 2750) is refused by name, in the line and the order
        refused(
            write(&mut tx, &line, quantity("1000")).await,
            ErrorKind::Access,
            &[approved_name.as_str()],
        );
        refused(
            write(
                &mut tx,
                &line,
                RawVals::new().set_cell("price_unit", Cell::Decimal(dec("1000"))),
            )
            .await,
            ErrorKind::Access,
            &[approved_name.as_str()],
        );
        let line_model = lines(&tx, &c);
        refused(
            try_create(
                &mut tx,
                &line_model,
                line_vals(k.bolt)
                    .set_cell("order_id", Cell::Id(approved_id))
                    .set_cell("product_qty", Cell::Decimal(dec("1000"))),
            )
            .await,
            ErrorKind::Access,
            &[approved_name.as_str()],
        );
        assert_eq!(num(&mut tx, &line, "product_qty").await, dec("1"));
        assert_eq!(num(&mut tx, &approved, "amount_total").await, dec("2.75"));
        // a raise inside the limit (10 bolts, 27.5) and a lower total pass
        write(&mut tx, &line, quantity("10")).await.unwrap();
        assert_eq!(num(&mut tx, &approved, "amount_total").await, dec("27.5"));
        write(&mut tx, &line, quantity("2")).await.unwrap();
        // an order waiting for approval stays editable
        let waiting = order(
            &mut tx,
            &c,
            header().set_commands(
                "order_line",
                [Command::Create(
                    line_vals(k.bolt).set_cell("product_qty", Cell::Decimal(dec("1000"))),
                )],
            ),
        )
        .await;
        act(&mut tx, &waiting, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &waiting).await, "to approve");
        let waiting_line = lines_of(&mut tx, &c, &waiting).await.remove(0);
        write(&mut tx, &waiting_line, quantity("2000"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        // an approver may raise it; the deadline of an approved order is refused, locked or not
        let mut tx = orm.begin(manager).await.unwrap();
        let approved = orders(&tx, &c).browse([approved_id]).unwrap();
        let line = lines_of(&mut tx, &c, &approved).await.remove(0);
        write(&mut tx, &line, quantity("1000")).await.unwrap();
        assert_eq!(num(&mut tx, &approved, "amount_total").await, dec("2750"));
        let before = cell(&mut tx, &approved, "date_order").await;
        let later = Cell::DateTime(DateTime::parse_odoo("2030-01-01 10:00:00").unwrap());
        refused(
            write(
                &mut tx,
                &approved,
                RawVals::new().set_cell("date_order", later.clone()),
            )
            .await,
            ErrorKind::Validation,
            &["date_order", approved_name.as_str()],
        );
        act(&mut tx, &approved, "button_lock").await.unwrap();
        refused(
            write(
                &mut tx,
                &approved,
                RawVals::new().set_cell("date_order", Cell::DateTime(DateTime::now())),
            )
            .await,
            ErrorKind::Validation,
            &["date_order", approved_name.as_str()],
        );
        assert_eq!(cell(&mut tx, &approved, "date_order").await, before);
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_new_product_or_vendor_prices_the_line_again() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        let env = tx.env().with_company(main).unwrap();
        let second = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Second vendor"))],
            )
            .await
            .unwrap()
            .ids()[0];
        env.records("product.supplierinfo")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(second))
                        .set_cell("product_id", Cell::Id(k.nut))
                        .set_cell("min_qty", Cell::Decimal(dec("0")))
                        .set_cell("price", Cell::Decimal(dec("7")))
                        .set_cell("delay", Cell::Int(1)),
                ],
            )
            .await
            .unwrap();
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        let in_order = |commands: Vec<RawVals>| {
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    commands
                        .into_iter()
                        .map(Command::Create)
                        .collect::<Vec<_>>(),
                )
        };
        // a nut priced by the vendor (4), a bolt, a fresh bolt and a fresh nut
        let po = order(
            &mut tx,
            &c,
            in_order(vec![
                line_vals(k.nut),
                line_vals(k.bolt),
                line_vals(k.bolt),
                line_vals(k.nut),
            ]),
        )
        .await;
        let found = lines_of(&mut tx, &c, &po).await;
        let (priced, hand, fresh_bolt, fresh_nut) = (&found[0], &found[1], &found[2], &found[3]);
        assert_eq!(num(&mut tx, priced, "price_unit").await, dec("4"));
        let product = |id: i64| RawVals::new().set_cell("product_id", Cell::Id(id));
        // the nut line becomes a bolt: everything of the new product, nothing of the nut
        write(&mut tx, priced, product(k.bolt)).await.unwrap();
        assert_eq!(
            shown(&mut tx, priced).await,
            shown(&mut tx, fresh_bolt).await
        );
        assert_eq!(
            cell(&mut tx, priced, "selected_seller_id").await,
            cell(&mut tx, fresh_bolt, "selected_seller_id").await
        );
        // a price set by hand stays on a new product; unit, taxes and description follow it
        write(
            &mut tx,
            hand,
            RawVals::new().set_cell("price_unit", Cell::Decimal(dec("9"))),
        )
        .await
        .unwrap();
        write(&mut tx, hand, product(k.nut)).await.unwrap();
        assert_eq!(num(&mut tx, hand, "price_unit").await, dec("9"));
        assert_eq!(
            cell(&mut tx, hand, "tax_ids").await,
            cell(&mut tx, fresh_nut, "tax_ids").await
        );
        assert_eq!(
            cell(&mut tx, hand, "name").await,
            cell(&mut tx, fresh_nut, "name").await
        );
        // a new vendor on the draft prices the lines again; the typed price stays
        let second_order = order(
            &mut tx,
            &c,
            in_order(vec![line_vals(k.nut), line_vals(k.nut)]),
        )
        .await;
        let found = lines_of(&mut tx, &c, &second_order).await;
        assert_eq!(num(&mut tx, &found[0], "price_unit").await, dec("4"));
        write(
            &mut tx,
            &found[1],
            RawVals::new().set_cell("price_unit", Cell::Decimal(dec("9"))),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &second_order,
            RawVals::new().set_cell("partner_id", Cell::Id(second)),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &found[0], "price_unit").await, dec("7"));
        assert_eq!(num(&mut tx, &found[1], "price_unit").await, dec("9"));
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn an_approved_order_in_a_foreign_currency_keeps_its_limit() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let usd = xmlid(&mut tx, "base", "USD").await;
        let eur = xmlid(&mut tx, "base", "EUR").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        let env = tx.env().clone();
        env.records("res.currency")
            .unwrap()
            .browse([eur])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await
            .unwrap();
        let create = async |tx: &mut Transaction, env: &Env, model: &str, vals: RawVals| -> i64 {
            env.records(model)
                .unwrap()
                .create(tx, vec![vals])
                .await
                .unwrap()
                .ids()[0]
        };
        let euro = create(
            &mut tx,
            &env,
            "res.company",
            RawVals::new()
                .set_cell("name", text("Euro company"))
                .set_cell("currency_id", Cell::Id(eur)),
        )
        .await;
        let rate = async |tx: &mut Transaction, currency: i64, value: &str, day: Date| {
            create(
                tx,
                &env.with_company(euro).unwrap(),
                "res.currency.rate",
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(currency))
                    .set_cell("company_id", Cell::Id(euro))
                    .set_cell("name", Cell::Date(day))
                    .set_cell("rate", Cell::Decimal(dec(value))),
            )
            .await;
        };
        let day = |y, m, d| Date::from_ymd(y, m, d).unwrap();
        // one dollar buys two euros from 2018
        rate(&mut tx, eur, "1", day(2018, 1, 1)).await;
        rate(&mut tx, usd, "0.5", day(2018, 1, 1)).await;
        let admin = user(&mut tx, "purchase-admin", &[system], &[main, euro]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main, euro]).await;
        let manager = user(&mut tx, "purchase-manager", &[manager_group], &[main, euro]).await;
        let vendor = create(
            &mut tx,
            &env,
            "res.partner",
            RawVals::new().set_cell("name", text("Vendor")),
        )
        .await;
        let crate_product = create(
            &mut tx,
            &env.with_company(main).unwrap(),
            "product.product",
            RawVals::new()
                .set_cell("name", text("Crate"))
                .set_cell("standard_price", Cell::Decimal(dec("10")))
                .set_cell("uom_id", Cell::Id(unit)),
        )
        .await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            euro,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(100.0)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);

        // an approved order of 10 dollars (20 euros, under the limit of 100)
        let both = [main, euro];
        let mut tx = orm.begin(manager).await.unwrap();
        let approved = order(
            &mut tx,
            &both,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_cell("company_id", Cell::Id(euro))
                .set_cell("currency_id", Cell::Id(usd))
                .set_cell(
                    "date_order",
                    Cell::DateTime(DateTime::parse_odoo("2019-05-31 12:00:00").unwrap()),
                )
                .set_commands(
                    "order_line",
                    [Command::Create(
                        line_vals(crate_product).set_cell("price_unit", Cell::Decimal(dec("10"))),
                    )],
                ),
        )
        .await;
        act(&mut tx, &approved, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &approved).await, "purchase");
        assert_eq!(num(&mut tx, &approved, "amount_total_cc").await, dec("20"));
        let approved_id = approved.ids()[0];
        let approved_name = name(&mut tx, &approved).await;
        tx.commit().await.unwrap();
        drop(tx);

        // the rate falls after the order was saved: ten euros to the dollar from 2019
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env().clone();
        create(
            &mut tx,
            &env.with_company(euro).unwrap(),
            "res.currency.rate",
            RawVals::new()
                .set_cell("currency_id", Cell::Id(usd))
                .set_cell("company_id", Cell::Id(euro))
                .set_cell("name", Cell::Date(day(2019, 1, 1)))
                .set_cell("rate", Cell::Decimal(dec("0.1"))),
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(buyer).await.unwrap();
        let approved = orders(&tx, &both).browse([approved_id]).unwrap();
        let line = lines_of(&mut tx, &both, &approved).await.remove(0);
        // the deadline cannot be moved to a day with another rate
        refused(
            write(
                &mut tx,
                &approved,
                RawVals::new().set_cell(
                    "date_order",
                    Cell::DateTime(DateTime::parse_odoo("2018-06-01 12:00:00").unwrap()),
                ),
            )
            .await,
            ErrorKind::Validation,
            &["date_order", approved_name.as_str()],
        );
        // a change that keeps the total in the order currency is no raise, though the rate moved
        write(
            &mut tx,
            &line,
            RawVals::new().set_cell("name", text("Wooden crate")),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &approved, "amount_total").await, dec("10"));
        assert_eq!(num(&mut tx, &approved, "amount_total_cc").await, dec("100"));
        // a real raise is asked for the limit at the rate of the deadline's day (300 euros for a price of 30)
        refused(
            write(
                &mut tx,
                &line,
                RawVals::new().set_cell("price_unit", Cell::Decimal(dec("30"))),
            )
            .await,
            ErrorKind::Access,
            &[approved_name.as_str()],
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn context_defaults_empty_tax_sets_and_a_tax_without_a_sequence() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let eur = xmlid(&mut tx, "base", "EUR").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        // the context keys are taken, not ignored
        let at = DateTime::parse_odoo("2031-02-03 04:05:06").unwrap();
        let model = orders(&tx, &c).with_context([
            ("default_date_order", Value::DateTime(at)),
            ("default_currency_id", Value::Int(eur)),
            ("default_user_id", Value::Int(buyer.get())),
        ]);
        let created = model
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("partner_id", Cell::Id(k.vendor))],
            )
            .await
            .unwrap();
        assert_eq!(
            cell(&mut tx, &created, "date_order").await,
            Cell::DateTime(at)
        );
        assert_eq!(cell(&mut tx, &created, "currency_id").await, Cell::Id(eur));
        assert_eq!(
            cell(&mut tx, &created, "user_id").await,
            Cell::Id(buyer.get())
        );
        let wrong = orders(&tx, &c).with_context([("default_currency_id", Value::str("EUR"))]);
        refused(
            try_create(
                &mut tx,
                &wrong,
                RawVals::new().set_cell("partner_id", Cell::Id(k.vendor)),
            )
            .await,
            ErrorKind::Validation,
            &["default_currency_id"],
        );

        // an empty tax set that was given is kept; a line created without the key takes the product's taxes
        let po = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [
                        Command::Create(line_vals(k.bolt)),
                        Command::Create(
                            line_vals(k.bolt).set_commands("tax_ids", [Command::Set(vec![])]),
                        ),
                    ],
                ),
        )
        .await;
        let found = lines_of(&mut tx, &c, &po).await;
        assert_eq!(num(&mut tx, &found[0], "price_tax").await, dec("0.25"));
        assert_eq!(num(&mut tx, &found[1], "price_tax").await, dec("0"));
        let copied = po.copy(&mut tx, None).await.unwrap();
        let copies = lines_of(&mut tx, &c, &copied).await;
        assert_eq!(num(&mut tx, &copies[0], "price_tax").await, dec("0.25"));
        assert_eq!(num(&mut tx, &copies[1], "price_tax").await, dec("0"));
        tx.commit().await.unwrap();
        drop(tx);

        // a tax without a sequence is refused by name
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute("UPDATE account_tax SET sequence = NULL", &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(buyer).await.unwrap();
        let plain = orders(&tx, &c);
        refused(
            try_create(
                &mut tx,
                &plain,
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(k.vendor))
                    .set_commands("order_line", [Command::Create(line_vals(k.bolt))]),
            )
            .await,
            ErrorKind::Validation,
            &["no sequence"],
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_hand_priced_line_loses_the_row_and_discount_of_the_former_product_or_vendor() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        user(&mut tx, "purchase-admin", &[system], &[main]).await;
        let buyer = user(&mut tx, "buyer", &[buyer_group], &[main]).await;
        let k = catalog(&mut tx, main).await;
        let env = tx.env().with_company(main).unwrap();
        let second = env
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Second vendor"))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let c = [main];
        let mut tx = orm.begin(buyer).await.unwrap();
        let twelve = || line_vals(k.nut).set_cell("product_qty", Cell::Decimal(dec("12")));
        let po = order(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [Command::Create(twelve()), Command::Create(twelve())],
                ),
        )
        .await;
        let found = lines_of(&mut tx, &c, &po).await;
        // twelve nuts: the vendor price of 3 with 10 % off, and its row
        assert_eq!(num(&mut tx, &found[0], "discount").await, dec("10"));
        assert_ne!(
            cell(&mut tx, &found[0], "selected_seller_id").await,
            Cell::Null
        );
        let by_hand =
            |price: &str| RawVals::new().set_cell("price_unit", Cell::Decimal(dec(price)));
        // a new product, with a price given in the same write
        write(
            &mut tx,
            &found[0],
            by_hand("9").set_cell("product_id", Cell::Id(k.bolt)),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &found[0], "price_unit").await, dec("9"));
        assert_eq!(num(&mut tx, &found[0], "discount").await, dec("0"));
        assert_eq!(
            cell(&mut tx, &found[0], "selected_seller_id").await,
            Cell::Null
        );
        // a new vendor on the order, the other line priced by hand
        write(&mut tx, &found[1], by_hand("9")).await.unwrap();
        write(
            &mut tx,
            &po,
            RawVals::new().set_cell("partner_id", Cell::Id(second)),
        )
        .await
        .unwrap();
        assert_eq!(num(&mut tx, &found[1], "price_unit").await, dec("9"));
        assert_eq!(num(&mut tx, &found[1], "discount").await, dec("0"));
        assert_eq!(
            cell(&mut tx, &found[1], "selected_seller_id").await,
            Cell::Null
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn approving_a_purchase_order_creates_one_direct_receipt() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_module(&db, "purchase_stock").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let k = catalog(&mut tx, main).await;
        let system_group = xmlid(&mut tx, "base", "group_system").await;
        let admin = user(&mut tx, "receipt-admin", &[system_group], &[main]).await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let manager_group = xmlid(&mut tx, "purchase", "group_purchase_manager").await;
        let buyer = user(&mut tx, "receipt-buyer", &[buyer_group], &[main]).await;
        let manager = user(&mut tx, "receipt-manager", &[manager_group], &[main]).await;
        let po = order(
            &mut tx,
            &[main],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [Command::Create(
                        line_vals(k.bolt)
                            .set_cell("uom_id", Cell::Id(k.dozen))
                            .set_cell("product_qty", Cell::Decimal(dec("2"))),
                    )],
                ),
        )
        .await;
        let direct = po.ids()[0];
        let direct_reference = name(&mut tx, &po).await;
        let mut second = line_vals(k.bolt).set_cell("product_qty", Cell::Decimal(dec("1")));
        second = second.set_cell("price_unit", Cell::Decimal(dec("2")));
        let waiting = order(
            &mut tx,
            &[main],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands("order_line", [Command::Create(second)]),
        )
        .await
        .ids()[0];
        tx.execute(
            "UPDATE purchase_order_line SET uom_id = NULL, date_planned = NULL WHERE order_id = $1",
            &[&waiting],
        )
        .await
        .unwrap();
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(buyer).await.unwrap();
        let po = orders(&tx, &[main]).browse([direct]).unwrap();
        assert_eq!(cell(&mut tx, &po, "direct_receipt_id").await, Cell::Null);
        act(&mut tx, &po, "button_confirm").await.unwrap();
        let original: i64 = tx
            .query(
                "SELECT picking_id FROM purchase_stock_receipt_link WHERE order_id=$1",
                &[&direct],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            cell(&mut tx, &po, "direct_receipt_id").await,
            Cell::Id(original)
        );
        assert_eq!(state(&mut tx, &po).await, "purchase");
        let error = act(&mut tx, &po, "button_confirm").await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!(
                "You can confirm a purchase order only in the state draft or sent; these are not: {direct_reference} (purchase)."
            )
        );
        assert_direct_receipt(&mut tx, direct, main, k.bolt, k.dozen, dec("2"), dec("24")).await;
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(admin).await.unwrap();
        set(
            &mut tx,
            main,
            vec![
                (APPROVAL_SETTING, Value::str("two_step")),
                (AMOUNT_SETTING, Value::Float(0.0)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(buyer).await.unwrap();
        let pending = orders(&tx, &[main]).browse([waiting]).unwrap();
        act(&mut tx, &pending, "button_confirm").await.unwrap();
        assert_eq!(state(&mut tx, &pending).await, "to approve");
        assert!(
            tx.query(
                "SELECT 1 FROM purchase_stock_receipt_link WHERE order_id=$1",
                &[&waiting]
            )
            .await
            .unwrap()
            .is_empty()
        );
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let pending = orders(&tx, &[main]).browse([waiting]).unwrap();
        let reference = name(&mut tx, &pending).await;
        tx.execute(
            "UPDATE stock_picking_type t SET active=false FROM stock_warehouse w WHERE w.company_id=$1 AND w.in_type_id=t.id",
            &[&main],
        ).await.unwrap();
        let error = act(&mut tx, &pending, "button_approve").await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            format!(
                "Purchase order {reference} needs the warehouse's active one-step incoming operation. Other receipt routes require a later release."
            )
        );
        assert_eq!(state(&mut tx, &pending).await, "to approve");
        assert!(
            tx.query(
                "SELECT 1 FROM purchase_stock_receipt_link WHERE order_id=$1",
                &[&waiting]
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            tx.query(
                "SELECT 1 FROM stock_picking WHERE company_id=$1 AND origin=$2",
                &[&main, &reference],
            )
            .await
            .unwrap()
            .is_empty()
        );
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(manager).await.unwrap();
        let pending = orders(&tx, &[main]).browse([waiting]).unwrap();
        act(&mut tx, &pending, "button_approve").await.unwrap();
        assert_direct_receipt(&mut tx, waiting, main, k.bolt, k.unit, dec("1"), dec("1")).await;
        act(&mut tx, &pending, "button_cancel").await.unwrap();
        assert_eq!(state(&mut tx, &pending).await, "cancel");
        let row = &tx.query("SELECT p.state FROM purchase_stock_receipt_link l JOIN stock_picking p ON p.id=l.picking_id WHERE l.order_id=$1", &[&waiting]).await.unwrap()[0];
        assert_eq!(row.get::<_, &str>(0), "cancel");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

async fn assert_direct_receipt(
    tx: &mut Transaction,
    order: i64,
    company: i64,
    product: i64,
    unit: i64,
    demand: Decimal,
    product_qty: Decimal,
) {
    let receipts = tx.query(
        "SELECT m.company_id, m.product_id, m.uom_id, m.product_uom_qty, m.product_qty,
                m.state, l.order_id, l.qty_received
         FROM purchase_stock_receipt_link r
         JOIN stock_picking p ON p.id = r.picking_id AND p.company_id = r.company_id
         JOIN stock_move m ON m.picking_id = p.id AND m.company_id = r.company_id
         JOIN purchase_stock_receipt_move_link ml ON ml.move_id = m.id AND ml.company_id = r.company_id
         JOIN purchase_order_line l ON l.id = ml.purchase_line_id
         WHERE r.order_id = $1",
        &[&order],
    ).await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "one receipt move for the confirmed purchase line"
    );
    assert_eq!(receipts[0].get::<_, i64>(0), company);
    assert_eq!(receipts[0].get::<_, i64>(1), product);
    assert_eq!(receipts[0].get::<_, i64>(2), unit);
    assert_eq!(receipts[0].get::<_, Decimal>(3), demand);
    assert_eq!(receipts[0].get::<_, Decimal>(4), product_qty);
    assert_ne!(receipts[0].get::<_, &str>(5), "done");
    assert_eq!(receipts[0].get::<_, i64>(6), order);
    assert_eq!(receipts[0].get::<_, Decimal>(7), dec("0"));
}

#[test]
fn original_receipt_is_visible_on_the_right_order_after_reopening() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_module(&db, "purchase_stock").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let k = catalog(&mut tx, main).await;
        let values = || {
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands("order_line", [Command::Create(line_vals(k.bolt))])
        };
        let confirmed = order(&mut tx, &[main], values()).await;
        let draft = order(&mut tx, &[main], values()).await;
        act(&mut tx, &confirmed, "button_confirm").await.unwrap();
        let confirmed_id = confirmed.ids()[0];
        let draft_id = draft.ids()[0];
        let picking: i64 = tx
            .query(
                "SELECT picking_id FROM purchase_stock_receipt_link WHERE order_id=$1",
                &[&confirmed_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        // Old imports may carry a different display reference. Only the immutable
        // association can determine which order should open the transfer.
        let other_reference = name(&mut tx, &draft).await;
        tx.execute(
            "UPDATE stock_picking SET origin=$1 WHERE id=$2",
            &[&other_reference, &picking],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let batch = orders(&tx, &[main])
            .browse([draft_id, confirmed_id])
            .unwrap();
        assert_eq!(
            batch.cells(&mut tx, "direct_receipt_id").await.unwrap(),
            [Cell::Null, Cell::Id(picking)]
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn direct_receipt_source_obeys_purchase_access_even_after_an_elevated_read() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_module(&db, "purchase_stock").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let catalog = catalog(&mut tx, main).await;
        let group = xmlid(&mut tx, "stock", "group_stock_user").await;
        let admin_group = xmlid(&mut tx, "base", "group_system").await;
        user(&mut tx, "receipt-source-admin", &[admin_group], &[main]).await;
        let clerk = user(&mut tx, "receipt-source-clerk", &[group], &[main]).await;
        let purchase = order(
            &mut tx,
            &[main],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(catalog.vendor))
                .set_commands("order_line", [Command::Create(line_vals(catalog.bolt))]),
        )
        .await;
        act(&mut tx, &purchase, "button_confirm").await.unwrap();
        let order_id = purchase.ids()[0];
        let [Cell::Id(picking_id)] =
            purchase.cells(&mut tx, "direct_receipt_id").await.unwrap()[..]
        else {
            panic!("The purchase order must have its direct receipt");
        };
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let receipt = tx
            .env()
            .with_company(main)
            .unwrap()
            .records("stock.picking")
            .unwrap()
            .browse([picking_id])
            .unwrap();
        assert_eq!(
            receipt.cells(&mut tx, "purchase_order_id").await.unwrap(),
            [Cell::Null]
        );
        assert_eq!(
            receipt
                .sudo(true)
                .cells(&mut tx, "purchase_order_id")
                .await
                .unwrap(),
            [Cell::Id(order_id)]
        );
        assert_eq!(
            receipt.cells(&mut tx, "purchase_order_id").await.unwrap(),
            [Cell::Null]
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn direct_receipt_requires_readable_demand_and_matching_stock_parents() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_module(&db, "purchase_stock").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = xmlid(&mut tx, "base", "main_company").await;
        let catalog = catalog(&mut tx, main).await;
        let admin_group = xmlid(&mut tx, "base", "group_system").await;
        user(&mut tx, "receipt-access-admin", &[admin_group], &[main]).await;
        let buyer_group = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let buyer = user(&mut tx, "receipt-access-buyer", &[buyer_group], &[main]).await;
        let env = tx.env().with_company(main).unwrap();
        let service = env
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Restricted service"))
                        .set_cell("type", text("service"))
                        .set_cell("uom_id", Cell::Id(catalog.unit)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let head = || RawVals::new().set_cell("partner_id", Cell::Id(catalog.vendor));
        let visible = order(
            &mut tx,
            &[main],
            head().set_commands("order_line", [Command::Create(line_vals(catalog.bolt))]),
        )
        .await;
        let protected = order(
            &mut tx,
            &[main],
            head().set_commands(
                "order_line",
                [
                    Command::Create(line_vals(catalog.bolt)),
                    Command::Create(line_vals(service)),
                ],
            ),
        )
        .await;
        let protected_name = name(&mut tx, &protected).await;
        let protected_order = protected.ids()[0];
        let visible_order = visible.ids()[0];
        let po_line_model = xmlid(&mut tx, "purchase", "model_purchase_order_line").await;
        env.records("ir.access")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Restricted source service fixture"))
                        .set_cell("model_id", Cell::Id(po_line_model))
                        .set_cell("operation", text("r"))
                        .set_cell(
                            "domain",
                            text(&format!("[('product_id', '!=', {service})]")),
                        ),
                ],
            )
            .await
            .unwrap();
        let default_type: i64 = tx
            .query(
                "SELECT in_type_id FROM stock_warehouse WHERE company_id=$1 AND active",
                &[&main],
            )
            .await
            .unwrap()[0]
            .get(0);
        let other_type = env
            .records("stock.picking.type")
            .unwrap()
            .browse([default_type])
            .unwrap()
            .copy(
                &mut tx,
                Some(
                    [
                        ("name".into(), Value::str("Secondary incoming operation")),
                        ("sequence_code".into(), Value::str("IN2")),
                    ]
                    .into_iter()
                    .collect(),
                ),
            )
            .await
            .unwrap()
            .ids()[0];
        let other_receipt = env
            .records("stock.picking")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("picking_type_id", Cell::Id(other_type))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let orm = purchase_policy_orm(&db, &orm, &[]).await;
        let mut tx = orm.begin(buyer).await.unwrap();
        let visible_lines = tx
            .env()
            .records("purchase.order.line")
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("order_id", Operator::Eq, protected_order).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            visible_lines.len(),
            1,
            "the buyer's policy must hide the service line"
        );
        let protected = orders(&tx, &[main]).browse([protected_order]).unwrap();
        let refusal = act(&mut tx, &protected, "button_confirm")
            .await
            .unwrap_err();
        assert_eq!(refusal.kind(), ErrorKind::Access);
        assert_eq!(
            refusal.message(),
            "Purchase receipt demand is not accessible."
        );
        assert_eq!(state(&mut tx, &protected).await, "draft");
        assert!(
            tx.query(
                "SELECT id FROM stock_picking WHERE company_id=$1 AND origin=$2",
                &[&main, &protected_name],
            )
            .await
            .unwrap()
            .is_empty()
        );

        let visible = orders(&tx, &[main]).browse([visible_order]).unwrap();
        act(&mut tx, &visible, "button_confirm").await.unwrap();
        let row = &tx
            .query(
                "SELECT r.picking_id, m.id FROM purchase_stock_receipt_link r \
             JOIN stock_move m ON m.picking_id=r.picking_id WHERE r.order_id=$1",
                &[&visible_order],
            )
            .await
            .unwrap()[0];
        let original_receipt: i64 = row.get(0);
        let move_id: i64 = row.get(1);
        let scope = tx.env().with_company(main).unwrap();
        let moves = scope.records("stock.move").unwrap();
        let new_move = moves.clone();
        let create_refusal = tx
            .savepoint(async move |tx| {
                new_move
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("picking_id", Cell::Id(other_receipt))
                                .set_cell("picking_type_id", Cell::Id(default_type))
                                .set_cell("product_id", Cell::Id(catalog.bolt))
                                .set_cell("product_uom_qty", Cell::Decimal(dec("1"))),
                        ],
                    )
                    .await
            })
            .await
            .map(|_| ())
            .expect_err("a move needs its own transfer's operation type");
        assert_eq!(create_refusal.kind(), ErrorKind::Access);
        assert_eq!(
            create_refusal.message(),
            "Purchase receipt access requires matching incoming stock relations."
        );
        let stock_move = moves.browse([move_id]).unwrap();
        let refusal = write(
            &mut tx,
            &stock_move,
            RawVals::new().set_cell("picking_id", Cell::Id(other_receipt)),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.kind(), ErrorKind::Access);
        assert_eq!(
            refusal.message(),
            "Purchase receipt access requires matching incoming stock relations."
        );
        assert_eq!(
            cell(&mut tx, &stock_move, "picking_id").await,
            Cell::Id(original_receipt)
        );
        assert!(
            tx.query(
                "SELECT id FROM stock_move WHERE picking_id=$1",
                &[&other_receipt],
            )
            .await
            .unwrap()
            .is_empty()
        );
        let detail_move = moves
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("picking_id", Cell::Id(other_receipt))
                        .set_cell("product_id", Cell::Id(catalog.bolt))
                        .set_cell("product_uom_qty", Cell::Decimal(dec("1"))),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let details = scope.records("stock.move.line").unwrap();
        let detail_refusal = tx
            .savepoint(async move |tx| {
                details
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("move_id", Cell::Id(detail_move))
                                .set_cell("picking_id", Cell::Id(original_receipt)),
                        ],
                    )
                    .await
            })
            .await
            .map(|_| ())
            .expect_err("an operation detail follows the move's transfer");
        assert_eq!(
            detail_refusal.kind(),
            ErrorKind::UserError,
            "{detail_refusal:?}"
        );
        assert_eq!(
            detail_refusal.message(),
            "Transfer of an operation line follows its stock move and cannot be changed; delete the line and add it on the other move."
        );
        assert!(
            tx.query(
                "SELECT id FROM stock_move_line WHERE move_id=$1",
                &[&detail_move],
            )
            .await
            .unwrap()
            .is_empty()
        );
        act(&mut tx, &visible, "button_cancel").await.unwrap();
        let status = &tx
            .query(
                "SELECT state FROM stock_picking WHERE id=$1",
                &[&original_receipt],
            )
            .await
            .unwrap()[0];
        assert_eq!(status.get::<_, &str>(0), "cancel");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// The `domain` of a smart button's window action.
fn action_domain(action: &Value) -> Value {
    let Value::Map(map) = action else {
        panic!("a window action is a map: {action:?}")
    };
    map.get("domain").cloned().unwrap()
}

fn leaf(field: &str, op: &str, value: Value) -> Value {
    Value::list([Value::str(field), Value::str(op), value])
}

/// The vendor's and the product's Purchase buttons count readable confirmed orders only, the
/// rule of the sales buttons (an RFQ and a cancelled order do not count), and open them by a
/// domain; a user without purchase rights counts none.
#[test]
fn the_vendor_and_product_buttons_count_readable_confirmed_orders() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        user(&mut tx, "purchase-count-admin", &[system], &[company]).await;
        let internal = xmlid(&mut tx, "base", "group_user").await;
        let clerk = user(&mut tx, "purchase-count-clerk", &[internal], &[company]).await;
        let k = catalog(&mut tx, company).await;
        let template: i64 = tx
            .query(
                "SELECT product_tmpl_id::int8 FROM product_product WHERE id = $1",
                &[&k.bolt],
            )
            .await
            .unwrap()[0]
            .get(0);
        for operation in [
            Some("button_confirm"),
            Some("button_confirm"),
            None,
            Some("button_cancel"),
        ] {
            let po = order(
                &mut tx,
                &[company],
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(k.vendor))
                    .set_commands(
                        "order_line",
                        [Command::Create(
                            line_vals(k.bolt).set_cell("product_qty", Cell::Decimal(dec("2"))),
                        )],
                    ),
            )
            .await;
            if let Some(operation) = operation {
                act(&mut tx, &po, operation).await.unwrap();
            }
        }
        let env = tx.env().with_company(company).unwrap();
        let vendor = env
            .records("res.partner")
            .unwrap()
            .browse([k.vendor])
            .unwrap();
        assert_eq!(
            vendor.cell(&mut tx, "purchase_order_count").await.unwrap(),
            Cell::Int(2)
        );
        let action: Value = vendor
            .call(&mut tx, "action_view_purchase_orders", ())
            .await
            .unwrap();
        assert_eq!(
            action_domain(&action),
            Value::list([
                Value::str("&"),
                leaf("partner_id", "=", Value::Int(k.vendor)),
                leaf("state", "=", Value::str("purchase")),
            ])
        );
        let product = env
            .records("product.template")
            .unwrap()
            .browse([template])
            .unwrap();
        assert_eq!(
            product.cell(&mut tx, "purchase_order_count").await.unwrap(),
            Cell::Int(2)
        );
        let action: Value = product
            .call(&mut tx, "action_view_purchases", ())
            .await
            .unwrap();
        assert_eq!(
            action_domain(&action),
            Value::list([
                Value::str("&"),
                leaf(
                    "order_line.product_id.product_tmpl_id",
                    "=",
                    Value::Int(template)
                ),
                leaf("state", "=", Value::str("purchase")),
            ])
        );
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        let vendor = env
            .records("res.partner")
            .unwrap()
            .browse([k.vendor])
            .unwrap();
        assert_eq!(
            vendor.cell(&mut tx, "purchase_order_count").await.unwrap(),
            Cell::Int(0)
        );
        let product = env
            .records("product.template")
            .unwrap()
            .browse([template])
            .unwrap();
        assert_eq!(
            product.cell(&mut tx, "purchase_order_count").await.unwrap(),
            Cell::Int(0)
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}
