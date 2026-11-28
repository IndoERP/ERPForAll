// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;

use erp_fields::{Cell, DateTime, ErrorKind, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, Records, SUPERUSER_ID, Transaction};
use erp_sdk::Value;
use erp_testkit::{TemplateDb, TestDb};

#[path = "support/install_defaults.rs"]
mod install_defaults;

#[path = "sale_order/crm_forecast.rs"]
mod crm_forecast;

#[path = "sale_order/crm_lifecycle.rs"]
mod crm_lifecycle;

#[path = "sale_order/crm.rs"]
mod crm;

#[path = "sale_order/lines.rs"]
mod lines;

#[path = "sale_order/moves.rs"]
mod moves;

#[path = "sale_order/prices.rs"]
mod prices;

#[path = "support/report_access.rs"]
mod report_access;
#[path = "support/report_frame.rs"]
mod report_frame;
#[path = "sale_order/reports.rs"]
mod reports;

#[path = "sale_order/screens.rs"]
mod screens;

#[path = "sale_order/delivery.rs"]
mod delivery;

fn system(db: &TestDb) -> ModuleSystem {
    system_with_stock(db, false)
}

fn system_with_stock(db: &TestDb, stock: bool) -> ModuleSystem {
    system_for_pool(db.pool().clone(), stock)
}

fn system_for_pool(pool: erp_testkit::deadpool_postgres::Pool, stock: bool) -> ModuleSystem {
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
    let builder = ModuleSystem::builder(pool, index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables());
    if stock {
        builder
            .code([
                &erp_addon_base::ADDON,
                &erp_addon_barcodes::ADDON,
                &erp_addon_barcodes_gs1_nomenclature::ADDON,
                &erp_addon_uom::ADDON,
                &erp_addon_mail::ADDON,
                &erp_addon_product::ADDON,
                &erp_addon_product_mail::ADDON,
                &erp_addon_web::ADDON,
                &erp_addon_analytic::ADDON,
                &erp_addon_account::ADDON,
                &erp_addon_account_mail::ADDON,
                &erp_addon_reporting::ADDON,
                &erp_addon_stock::ADDON,
                &erp_addon_stock_account::ADDON,
                &erp_addon_valuation::ADDON,
                &erp_addon_stock_mail::ADDON,
                &erp_addon_sales_team::ADDON,
                &erp_addon_sale::ADDON,
                &erp_addon_sale_stock::ADDON,
            ])
            .require_sql_objects([
                &erp_addon_account::reconcile::history_guard::MANIFEST,
                &erp_addon_stock::schema_guards::MANIFEST,
                &erp_addon_stock_account::history_guards::MANIFEST,
                &erp_addon_valuation::MANIFEST,
            ])
            .require_sql_uninstall_guards([
                &erp_addon_account::reconcile::history_guard::UNINSTALL,
                &erp_addon_stock_account::uninstall_guard::MANIFEST,
                &erp_addon_valuation::lifecycle::MANIFEST,
            ])
    } else {
        builder
            .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
            .require_sql_uninstall_guards([&erp_addon_account::reconcile::history_guard::UNINSTALL])
            .code([
                &erp_addon_base::ADDON,
                &erp_addon_web::ADDON,
                &erp_addon_mail::ADDON,
                &erp_addon_uom::ADDON,
                &erp_addon_product::ADDON,
                // auto-installed with product and mail, which sale brings in
                &erp_addon_product_mail::ADDON,
                &erp_addon_analytic::ADDON,
                &erp_addon_account::ADDON,
                // auto-installed with account and mail, which sale brings in
                &erp_addon_account_mail::ADDON,
                &erp_addon_reporting::ADDON,
                &erp_addon_sales_team::ADDON,
                &erp_addon_sale::ADDON,
                &erp_addon_crm::ADDON,
                &erp_addon_sale_crm::ADDON,
            ])
    }
    .build()
    .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
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

async fn install(db: &TestDb) -> Orm {
    install_system(db).await.1
}

async fn install_system(db: &TestDb) -> (ModuleSystem, Orm) {
    let system = system(db);
    let initialized = Box::pin(system.initialize(InitOptions::new()))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = Box::pin(system.button_immediate_install(
        &system.system_env(Context::new()),
        &[ModuleName::new("sale").unwrap()],
    ))
    .await
    .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    let orm = installed.orm;
    (system, orm)
}

/// Clone a cached `sale`-installed template database and load its [`ModuleSystem`] and [`Orm`].
async fn cloned_sale_system() -> (TestDb, ModuleSystem, Orm) {
    let db = TemplateDb::clone_or_init("erp-suite-sale:sale:v1", |pool| async move {
        let system = system_for_pool(pool, false);
        let initialized = Box::pin(system.initialize(InitOptions::new()))
            .await
            .map_err(|e| e.to_string())?;
        if !initialized.post_commit.is_clean() {
            return Err(format!(
                "initialize post_commit not clean: {:?}",
                initialized.post_commit
            ));
        }
        let installed = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("sale").unwrap()],
        ))
        .await
        .map_err(|e| e.to_string())?;
        if !installed.post_commit.is_clean() {
            return Err(format!(
                "install post_commit not clean: {:?}",
                installed.post_commit
            ));
        }
        Ok::<(), String>(())
    })
    .await
    .unwrap();
    let system = system(&db);
    let loaded = Box::pin(system.load()).await.unwrap();
    (db, system, loaded.orm)
}

async fn cloned_sale() -> (TestDb, Orm) {
    let (db, _system, orm) = cloned_sale_system().await;
    (db, orm)
}

/// The installed ORM with the database's compiled access policy, as a served node builds it: a
/// transaction of a user other than the system principal needs it for group checks and access
/// rules. Build it after the fixture commit that adds the users.
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

async fn xmlid(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module = $1::text AND name = $2::text",
        &[&module, &name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

fn id(value: i64) -> RecId {
    RecId::from_real(value).unwrap()
}

/// What every case needs: the main company and a customer.
struct World {
    company: i64,
    customer: i64,
}

async fn world(tx: &mut Transaction) -> World {
    let company = xmlid(tx, "base", "main_company").await;
    let customer = partner(
        tx,
        RawVals::new().set_cell("name", Cell::Str("Azure".into())),
    )
    .await;
    World { company, customer }
}

async fn partner(tx: &mut Transaction, vals: RawVals) -> i64 {
    // account's company-dependent partner fields need a current company
    let company = xmlid(tx, "base", "main_company").await;
    tx.env()
        .with_company(company)
        .unwrap()
        .records("res.partner")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

/// An active settings administrator: the base rule refuses a change of users or their groups
/// while the database has none.
async fn admin(tx: &mut Transaction) {
    let company = xmlid(tx, "base", "main_company").await;
    let system = xmlid(tx, "base", "group_system").await;
    let internal = xmlid(tx, "base", "group_user").await;
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Sales test administrator".into()))
                    .set_cell("login", Cell::Str("sale-admin".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands("company_ids", [Command::set([id(company)])])
                    .set_commands("group_ids", [Command::set([id(system), id(internal)])]),
            ],
        )
        .await
        .unwrap();
}

/// An internal user of the main company in `group` (an xmlid of `sales_team`).
async fn user(tx: &mut Transaction, login: &str, group: &str) -> i64 {
    let company = xmlid(tx, "base", "main_company").await;
    let group = xmlid(tx, "sales_team", group).await;
    let partner = partner(tx, RawVals::new().set_cell("name", Cell::Str(login.into()))).await;
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(login.into()))
                    .set_cell("login", Cell::Str(login.into()))
                    .set_cell("partner_id", Cell::Id(partner))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands("company_ids", [Command::set([id(company)])])
                    .set_commands("group_ids", [Command::set([id(group)])]),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn team(tx: &mut Transaction, name: &str, member: i64) -> i64 {
    let team = tx
        .env()
        .records("crm.team")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.env()
        .records("crm.team.member")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("crm_team_id", Cell::Id(team))
                    .set_cell("user_id", Cell::Id(member)),
            ],
        )
        .await
        .unwrap();
    team
}

async fn order(tx: &mut Transaction, world: &World, extra: RawVals) -> Records {
    let vals = extra
        .set_cell("partner_id", Cell::Id(world.customer))
        .set_cell("company_id", Cell::Id(world.company));
    tx.env()
        .records("sale.order")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
}

async fn new_order(tx: &mut Transaction, world: &World) -> Records {
    order(tx, world, RawVals::new()).await
}

async fn act(
    tx: &mut Transaction,
    orders: &Records,
    operation: &'static str,
) -> erp_fields::Result<()> {
    let orders = orders.clone();
    tx.savepoint(async move |tx| orders.call::<(), ()>(tx, operation, ()).await)
        .await
}

async fn text(tx: &mut Transaction, orders: &Records, field: &str) -> String {
    match orders.cell(tx, field).await.unwrap() {
        Cell::Str(value) => value.to_string(),
        other => panic!("{field} is {other:?}"),
    }
}

async fn flag(tx: &mut Transaction, orders: &Records, field: &str) -> bool {
    match orders.cell(tx, field).await.unwrap() {
        Cell::Bool(value) => value,
        other => panic!("{field} is {other:?}"),
    }
}

async fn many2one(tx: &mut Transaction, orders: &Records, field: &str) -> Option<i64> {
    match orders.cell(tx, field).await.unwrap() {
        Cell::Id(value) => Some(value),
        Cell::Null => None,
        other => panic!("{field} is {other:?}"),
    }
}

fn refused(result: erp_fields::Result<()>, kind: ErrorKind, message: &str) {
    let error = result.expect_err(message);
    assert_eq!(error.kind(), kind, "{error:?}");
    assert!(
        error.message().contains(message),
        "expected {message:?} in {:?}",
        error.message()
    );
}

async fn write(tx: &mut Transaction, orders: &Records, vals: RawVals) -> erp_fields::Result<()> {
    let orders = orders.clone();
    tx.savepoint(async move |tx| {
        orders.write(tx, vals).await?;
        tx.flush().await
    })
    .await
}

/// The global setting `key` set to `value` with the framework's rights.
async fn set_setting(tx: &mut Transaction, key: &str, value: Value) {
    use erp_addon_base::ir_config_setting::{read_values, write_values};
    use erp_sdk::settings::SettingsScope;
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

/// The installed defaults: the order sequence and the administrator's access row (B5).
#[test]
fn sale_installs_its_sequence_and_access_row() {
    run(async {
        let db = TestDb::create().await.unwrap();
        install(&db).await;
        let client = db.client().await.unwrap();
        for (sql, expected) in install_defaults::SALE {
            let rows: Vec<String> = client
                .query(*sql, &[])
                .await
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(rows, *expected, "{sql}");
        }
    });
}

#[test]
fn a_new_quotation_is_a_numbered_draft() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let row = tx
            .query(
                "SELECT prefix, padding::int8 FROM ir_sequence WHERE code = $1::text",
                &[&erp_addon_sale::SEQUENCE_CODE],
            )
            .await
            .unwrap();
        let (prefix, padding): (String, i64) = (row[0].get(0), row[0].get(1));
        let first = new_order(&mut tx, &world).await;
        let placeholder = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("name", Cell::Str(erp_addon_sale::PLACEHOLDER.into())),
        )
        .await;
        let (first_name, second_name) = (
            text(&mut tx, &first, "name").await,
            text(&mut tx, &placeholder, "name").await,
        );
        let number = |name: &str| -> i64 {
            let digits = name.strip_prefix(prefix.as_str()).unwrap();
            assert_eq!(digits.len(), usize::try_from(padding).unwrap(), "{name}");
            digits.parse().unwrap()
        };
        assert_eq!(number(&second_name), number(&first_name) + 1);
        // an explicit name is kept, without a number drawn
        let imported = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("name", Cell::Str("IMPORT-7".into())),
        )
        .await;
        assert_eq!(text(&mut tx, &imported, "name").await, "IMPORT-7");
        let after = new_order(&mut tx, &world).await;
        assert_eq!(
            number(&text(&mut tx, &after, "name").await),
            number(&second_name) + 1
        );
        assert_eq!(text(&mut tx, &first, "state").await, "draft");
        assert!(!flag(&mut tx, &first, "locked").await);
        tx.rollback().await.unwrap();
    });
}

#[test]
fn an_order_without_a_sequence_is_refused() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let sequence = xmlid(&mut tx, "sale", "seq_sale_order").await;
        tx.env()
            .records("ir.sequence")
            .unwrap()
            .browse(vec![sequence])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap();
        let orders = tx.env().records("sale.order").unwrap();
        let vals = RawVals::new()
            .set_cell("partner_id", Cell::Id(world.customer))
            .set_cell("company_id", Cell::Id(world.company));
        let result = tx
            .savepoint(async |tx| orders.create(tx, vec![vals.clone()]).await.map(|_| ()))
            .await;
        refused(
            result,
            ErrorKind::UserError,
            "No sequence with the code \"sale.order\" exists for the company",
        );
        let saved = orders
            .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap();
        assert_eq!(saved, 0, "no order is saved without its number");
        // an explicit name needs no sequence
        let named = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("name", Cell::Str("MANUAL-1".into())),
        )
        .await;
        assert_eq!(text(&mut tx, &named, "name").await, "MANUAL-1");
        tx.rollback().await.unwrap();
    });
}

#[test]
fn send_and_confirm_follow_the_transition_table() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let quotation = new_order(&mut tx, &world).await;
        act(&mut tx, &quotation, "action_quotation_sent")
            .await
            .unwrap();
        assert_eq!(text(&mut tx, &quotation, "state").await, "sent");
        refused(
            act(&mut tx, &quotation, "action_quotation_sent").await,
            ErrorKind::Validation,
            "is marked as sent only while it is a draft",
        );
        let before = DateTime::now();
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        assert_eq!(text(&mut tx, &quotation, "state").await, "sale");
        match quotation.cell(&mut tx, "date_order").await.unwrap() {
            Cell::DateTime(confirmed) => assert!(confirmed >= before, "{confirmed:?}"),
            other => panic!("date_order is {other:?}"),
        }
        assert!(
            !flag(&mut tx, &quotation, "locked").await,
            "the lock switch is off"
        );
        refused(
            act(&mut tx, &quotation, "action_confirm").await,
            ErrorKind::Validation,
            "Only a draft or sent quotation can be confirmed",
        );
        // one order in a wrong state refuses the whole call
        let draft = new_order(&mut tx, &world).await;
        let both = draft.browse_ids(
            draft
                .rec_ids()
                .iter()
                .chain(quotation.rec_ids().iter())
                .copied(),
        );
        refused(
            act(&mut tx, &both, "action_confirm").await,
            ErrorKind::Validation,
            &text(&mut tx, &quotation, "name").await,
        );
        assert_eq!(text(&mut tx, &draft, "state").await, "draft");
        tx.rollback().await.unwrap();
    });
}

#[test]
fn the_lock_setting_locks_confirmed_orders() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        set_setting(&mut tx, erp_addon_sale::LOCK_CONFIRMED, Value::Bool(true)).await;
        let quotation = new_order(&mut tx, &world).await;
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        assert_eq!(text(&mut tx, &quotation, "state").await, "sale");
        assert!(flag(&mut tx, &quotation, "locked").await);
        tx.rollback().await.unwrap();
    });
}

#[test]
fn late_confirmation_refusal_rolls_back_the_batch_for_an_orm_caller() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let draft = new_order(&mut tx, &world).await;
        let sent = new_order(&mut tx, &world).await;
        act(&mut tx, &sent, "action_quotation_sent").await.unwrap();
        let records = draft.browse([draft.ids()[0], sent.ids()[0]]).unwrap();
        let draft_date = draft.cell(&mut tx, "date_order").await.unwrap();
        let sent_date = sent.cell(&mut tx, "date_order").await.unwrap();

        // An invalid stored switch returns a regular validation error during the final
        // confirmation step, after the order writes and hook. Do not wrap the call in act().
        let key = erp_addon_sale::LOCK_CONFIRMED;
        set_setting(&mut tx, key, Value::Bool(true)).await;
        assert_eq!(
            tx.execute(
                "UPDATE ir_config_setting_value SET value_text = '123' WHERE setting_key = $1",
                &[&key],
            )
            .await
            .unwrap(),
            1
        );
        let error = records
            .call::<(), ()>(&mut tx, "action_confirm", ())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Stored setting sale.lock_confirmed_sales is invalid."
        );
        assert_eq!(text(&mut tx, &draft, "state").await, "draft");
        assert_eq!(text(&mut tx, &sent, "state").await, "sent");
        assert_eq!(draft.cell(&mut tx, "date_order").await.unwrap(), draft_date);
        assert_eq!(sent.cell(&mut tx, "date_order").await.unwrap(), sent_date);
        assert!(!flag(&mut tx, &draft, "locked").await);
        assert!(!flag(&mut tx, &sent, "locked").await);

        // Earlier caller work and the transaction survive. Repair the fixture and retry.
        assert_eq!(
            tx.execute(
                "UPDATE ir_config_setting_value SET value_text = 'true' WHERE setting_key = $1",
                &[&key],
            )
            .await
            .unwrap(),
            1
        );
        records
            .call::<(), ()>(&mut tx, "action_confirm", ())
            .await
            .unwrap();
        assert_eq!(text(&mut tx, &draft, "state").await, "sale");
        assert_eq!(text(&mut tx, &sent, "state").await, "sale");
        assert!(flag(&mut tx, &draft, "locked").await);
        assert!(flag(&mut tx, &sent, "locked").await);
        tx.rollback().await.unwrap();
    });
}

#[test]
fn cancel_reset_lock_and_unlock() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let confirmed = new_order(&mut tx, &world).await;
        act(&mut tx, &confirmed, "action_confirm").await.unwrap();
        refused(
            act(&mut tx, &confirmed, "action_draft").await,
            ErrorKind::Validation,
            "Only a sent quotation or a cancelled order can be set back to draft",
        );
        act(&mut tx, &confirmed, "action_lock").await.unwrap();
        assert!(flag(&mut tx, &confirmed, "locked").await);
        // already locked: the end state, nothing to do
        act(&mut tx, &confirmed, "action_lock").await.unwrap();
        refused(
            act(&mut tx, &confirmed, "action_cancel").await,
            ErrorKind::UserError,
            "A locked sales order cannot be cancelled",
        );
        act(&mut tx, &confirmed, "action_unlock").await.unwrap();
        assert!(!flag(&mut tx, &confirmed, "locked").await);
        act(&mut tx, &confirmed, "action_cancel").await.unwrap();
        assert_eq!(text(&mut tx, &confirmed, "state").await, "cancel");
        act(&mut tx, &confirmed, "action_cancel").await.unwrap();
        act(&mut tx, &confirmed, "action_draft").await.unwrap();
        assert_eq!(text(&mut tx, &confirmed, "state").await, "draft");
        refused(
            act(&mut tx, &confirmed, "action_draft").await,
            ErrorKind::Validation,
            "Only a sent quotation or a cancelled order can be set back to draft",
        );
        refused(
            act(&mut tx, &confirmed, "action_lock").await,
            ErrorKind::Validation,
            "Only a confirmed sales order can be locked",
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn only_a_sales_administrator_locks_and_unlocks() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        admin(&mut tx).await;
        let salesman = user(&mut tx, "sam", "group_sale_salesman_all_leads").await;
        let confirmed = new_order(&mut tx, &world).await;
        act(&mut tx, &confirmed, "action_confirm").await.unwrap();
        let ids = confirmed.ids();
        tx.commit().await.unwrap();
        let orm = policy_orm(&db, &orm).await;

        let mut tx = orm
            .begin(erp_orm::UserId::new(salesman).unwrap())
            .await
            .unwrap();
        let theirs = tx
            .env()
            .records("sale.order")
            .unwrap()
            .browse(ids.clone())
            .unwrap();
        for operation in ["action_lock", "action_unlock"] {
            refused(
                act(&mut tx, &theirs, operation).await,
                ErrorKind::Access,
                "Only a Sales Administrator can lock or unlock sales orders.",
            );
        }
        tx.rollback().await.unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let ours = tx.env().records("sale.order").unwrap().browse(ids).unwrap();
        assert!(!flag(&mut tx, &ours, "locked").await);
        tx.rollback().await.unwrap();
    });
}

#[test]
fn only_quotations_and_cancelled_orders_are_deleted() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let sent = new_order(&mut tx, &world).await;
        act(&mut tx, &sent, "action_quotation_sent").await.unwrap();
        let result = tx.savepoint(async |tx| sent.unlink(tx).await).await;
        refused(
            result,
            ErrorKind::Validation,
            "Only a quotation or a cancelled order can be deleted",
        );
        let draft = new_order(&mut tx, &world).await;
        tx.savepoint(async |tx| draft.unlink(tx).await)
            .await
            .unwrap();
        act(&mut tx, &sent, "action_cancel").await.unwrap();
        tx.savepoint(async |tx| sent.unlink(tx).await)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    });
}

#[test]
fn client_writes_follow_the_header_rules() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let other = partner(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Other".into())),
        )
        .await;
        let quotation = new_order(&mut tx, &world).await;
        for (field, value) in [
            ("state", Cell::Str("sale".into())),
            ("locked", Cell::Bool(true)),
            ("name", Cell::Str("S99999".into())),
        ] {
            refused(
                write(&mut tx, &quotation, RawVals::new().set_cell(field, value)).await,
                ErrorKind::Validation,
                "of a sales order cannot be written directly.",
            );
        }
        let orders = tx.env().records("sale.order").unwrap();
        let born_sold = RawVals::new()
            .set_cell("partner_id", Cell::Id(world.customer))
            .set_cell("company_id", Cell::Id(world.company))
            .set_cell("state", Cell::Str("sale".into()));
        let result = tx
            .savepoint(async |tx| orders.create(tx, vec![born_sold.clone()]).await.map(|_| ()))
            .await;
        refused(result, ErrorKind::Validation, "cannot be written directly.");

        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        let name = text(&mut tx, &quotation, "name").await;
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("partner_id", Cell::Id(other)),
            )
            .await,
            ErrorKind::Validation,
            &format!(
                "The customer of the sales order {name} cannot be changed after it is confirmed."
            ),
        );
        act(&mut tx, &quotation, "action_lock").await.unwrap();
        for field in ["partner_invoice_id", "partner_shipping_id"] {
            refused(
                write(
                    &mut tx,
                    &quotation,
                    RawVals::new().set_cell(field, Cell::Id(other)),
                )
                .await,
                ErrorKind::Validation,
                &format!("The sales order {name} is locked. These fields cannot be changed: "),
            );
        }
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("commitment_date", Cell::DateTime(DateTime::now())),
            )
            .await,
            ErrorKind::Validation,
            "is locked",
        );
        write(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("client_order_ref", Cell::Str("PO-12".into()))
                .set_cell("origin", Cell::Str("Fair".into())),
        )
        .await
        .unwrap();
        act(&mut tx, &quotation, "action_unlock").await.unwrap();
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("partner_shipping_id", Cell::Id(other)),
        )
        .await
        .unwrap();
        act(&mut tx, &quotation, "action_cancel").await.unwrap();
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("client_order_ref", Cell::Str("X".into())),
            )
            .await,
            ErrorKind::Validation,
            &format!("The sales order {name} is cancelled and cannot be changed."),
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn addresses_come_from_the_customers_contacts() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let mut world = world(&mut tx).await;
        let company = partner(
            &mut tx,
            RawVals::new()
                .set_cell("name", Cell::Str("Deco".into()))
                .set_cell("is_company", Cell::Bool(true)),
        )
        .await;
        let contact = |name: &str, kind: &str| {
            RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("type", Cell::Str(kind.into()))
                .set_cell("parent_id", Cell::Id(company))
        };
        let billing = partner(&mut tx, contact("Deco billing", "invoice")).await;
        let shipping = partner(&mut tx, contact("Deco dock", "delivery")).await;
        world.customer = company;
        let quotation = new_order(&mut tx, &world).await;
        assert_eq!(
            many2one(&mut tx, &quotation, "partner_invoice_id").await,
            Some(billing)
        );
        assert_eq!(
            many2one(&mut tx, &quotation, "partner_shipping_id").await,
            Some(shipping)
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn salesperson_and_team_follow_the_customer_and_the_salesperson() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let mut world = world(&mut tx).await;
        admin(&mut tx).await;
        let ana = user(&mut tx, "ana", "group_sale_salesman").await;
        let ben = user(&mut tx, "ben", "group_sale_salesman").await;
        let north = team(&mut tx, "North", ana).await;
        let south = team(&mut tx, "South", ben).await;
        world.customer = partner(
            &mut tx,
            RawVals::new()
                .set_cell("name", Cell::Str("Gemini".into()))
                .set_cell("user_id", Cell::Id(ana)),
        )
        .await;
        let quotation = new_order(&mut tx, &world).await;
        assert_eq!(many2one(&mut tx, &quotation, "user_id").await, Some(ana));
        assert_eq!(many2one(&mut tx, &quotation, "team_id").await, Some(north));
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("user_id", Cell::Id(ben)),
        )
        .await
        .unwrap();
        assert_eq!(many2one(&mut tx, &quotation, "team_id").await, Some(south));
        tx.rollback().await.unwrap();
    });
}

#[test]
fn committed_orders_draw_distinct_numbers() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut names = Vec::new();
        for _ in 0..2 {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let world = world(&mut tx).await;
            let created = new_order(&mut tx, &world).await;
            names.push(text(&mut tx, &created, "name").await);
            tx.commit().await.unwrap();
        }
        assert_ne!(names[0], names[1]);
    });
}

/// An administrator's prefix of the order sequence survives an upgrade of `sale` (B5).
#[test]
fn an_edited_order_sequence_survives_the_upgrade() {
    run(async {
        let (_db, system, orm) = cloned_sale_system().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let sequence = xmlid(&mut tx, "sale", "seq_sale_order").await;
        let sequences = tx
            .env()
            .records("ir.sequence")
            .unwrap()
            .browse(vec![sequence])
            .unwrap();
        sequences
            .write(
                &mut tx,
                RawVals::new().set_cell("prefix", Cell::Str("SO-".into())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let env = orm.env(SUPERUSER_ID, Context::new());
        let upgraded =
            Box::pin(system.button_immediate_upgrade(&env, &[ModuleName::new("sale").unwrap()]))
                .await
                .unwrap();
        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let sequences = tx
            .env()
            .records("ir.sequence")
            .unwrap()
            .browse(vec![sequence])
            .unwrap();
        assert_eq!(
            sequences.cell(&mut tx, "prefix").await.unwrap(),
            Cell::Str("SO-".into())
        );
        let world = world(&mut tx).await;
        let created = new_order(&mut tx, &world).await;
        assert!(text(&mut tx, &created, "name").await.starts_with("SO-"));
        tx.rollback().await.unwrap();
    });
}

#[test]
fn status_fields_and_context_defaults_are_refused_on_orders() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let quotation = new_order(&mut tx, &world).await;
        let orders = tx.env().records("sale.order").unwrap();
        let minimal = RawVals::new()
            .set_cell("partner_id", Cell::Id(world.customer))
            .set_cell("company_id", Cell::Id(world.company));
        let statuses = [
            ("invoicing_closed", Cell::Bool(true), Value::Bool(true)),
            (
                "invoice_status",
                Cell::Str("invoiced".into()),
                Value::Str("invoiced".into()),
            ),
            (
                "delivery_status",
                Cell::Str("full".into()),
                Value::Str("full".into()),
            ),
        ];
        for (field, cell, _) in statuses.clone() {
            refused(
                write(
                    &mut tx,
                    &quotation,
                    RawVals::new().set_cell(field, cell.clone()),
                )
                .await,
                ErrorKind::Validation,
                "of a sales order cannot be written directly.",
            );
            for model in [orders.clone(), orders.sudo(true)] {
                let vals = minimal.clone().set_cell(field, cell.clone());
                let result = tx
                    .savepoint(async |tx| model.create(tx, vec![vals.clone()]).await.map(|_| ()))
                    .await;
                refused(result, ErrorKind::Validation, "cannot be written directly.");
            }
        }
        let defaults = [
            ("state", Value::Str("sale".into())),
            ("locked", Value::Bool(true)),
        ]
        .into_iter()
        .chain(statuses.into_iter().map(|(field, _, value)| (field, value)));
        for (field, value) in defaults {
            for model in [orders.clone(), orders.sudo(true)] {
                let model = model.with_context([(format!("default_{field}"), value.clone())]);
                let result = tx
                    .savepoint(async |tx| model.create(tx, vec![minimal.clone()]).await.map(|_| ()))
                    .await;
                refused(
                    result,
                    ErrorKind::Validation,
                    &format!("The context key default_{field} is not accepted"),
                );
            }
        }
        assert!(!flag(&mut tx, &quotation, "invoicing_closed").await);
        tx.rollback().await.unwrap();
    });
}

/// How many `user_id` columns `res_partner` has (0 or 1).
async fn partner_user_column(client: &erp_testkit::tokio_postgres::Client) -> i64 {
    client
        .query_one(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = \
             current_schema() AND table_name = 'res_partner' AND column_name = 'user_id'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// A database whose `res_partner` table predates the salesperson column (base 1.14: no column,
/// no field row) gets it from the upgrade of base, and orders read it afterwards.
#[test]
fn the_base_upgrade_adds_the_partner_salesperson_column() {
    run(async {
        let (db, system, orm) = cloned_sale_system().await;
        let client = db.client().await.unwrap();
        assert_eq!(partner_user_column(&client).await, 1);
        // the shape before base 1.15, in this test's own database
        client
            .batch_execute(
                "BEGIN;
                 CREATE TEMP TABLE later_fields ON COMMIT DROP AS
                     SELECT id FROM ir_model_fields WHERE model = 'res.partner' AND name = 'user_id';
                 DELETE FROM ir_model_data WHERE model = 'ir.model.fields'
                     AND res_id IN (SELECT id FROM later_fields);
                 DELETE FROM ir_model_fields WHERE id IN (SELECT id FROM later_fields);
                 ALTER TABLE res_partner DROP COLUMN user_id;
                 COMMIT;",
            )
            .await
            .unwrap();
        assert_eq!(partner_user_column(&client).await, 0);
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
        assert_eq!(partner_user_column(&client).await, 1);
        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let created = new_order(&mut tx, &world).await;
        assert_eq!(text(&mut tx, &created, "state").await, "draft");
        tx.rollback().await.unwrap();
    });
}

/// The sale order form opts its lines into the client's inline add (the x2many widget reads
/// `context={'inline_search_field': 'product_id'}` on the field): "Add a line" opens the inline
/// product search on the embedded list, the same mechanism the purchase order form uses,
/// instead of only the line form dialog.
#[test]
fn sale_order_form_offers_inline_line_entry_like_purchase_orders() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
        admin(&mut setup).await;
        let salesman = user(&mut setup, "inline-salesman", "group_sale_salesman").await;
        setup.commit().await.unwrap();
        drop(setup);
        let secured = policy_orm(&db, &orm).await;
        let mut tx = secured
            .begin(erp_sdk::UserId::new(salesman).unwrap())
            .await
            .unwrap();
        let requests = ["list", "form"]
            .into_iter()
            .map(|kind| Value::list([Value::False, Value::str(kind)]))
            .collect();
        let views = erp_addon_web::metadata_test_support::test_get_views(
            &tx.env(),
            &mut tx,
            "sale.order",
            requests,
            32 * 1024 * 1024,
        )
        .await
        .expect("the served sale.order views");
        let text = serde_json::to_string(&views).unwrap();
        assert!(
            text.contains("\"inline_search_field\""),
            "the served sale.order form must pass inline_search_field=product_id on its order_line field: {text}"
        );
        tx.rollback().await.unwrap();
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

/// The contact's and the product's Sales buttons count readable confirmed orders only, the rule
/// of the purchase buttons (a quotation does not count), and open them by a domain; a salesman
/// whose rule hides every one of them counts none.
#[test]
fn the_partner_and_product_buttons_count_readable_confirmed_orders() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        admin(&mut tx).await;
        let other = user(&mut tx, "sales-count-other", "group_sale_salesman").await;
        let seller = user(&mut tx, "sales-count-seller", "group_sale_salesman").await;
        let product = tx
            .env()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Desk".into()))
                        .set_cell("type", Cell::Str("consu".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let template: i64 = tx
            .query(
                "SELECT product_tmpl_id::int8 FROM product_product WHERE id = $1",
                &[&product],
            )
            .await
            .unwrap()[0]
            .get(0);
        for confirm in [true, true, false] {
            let placed = order(
                &mut tx,
                &world,
                RawVals::new()
                    .set_cell("user_id", Cell::Id(other))
                    .set_commands(
                        "order_line",
                        [Command::create(
                            RawVals::new()
                                .set_cell("product_id", Cell::Id(product))
                                .set_cell(
                                    "product_uom_qty",
                                    Cell::Decimal(erp_fields::Decimal::from(2)),
                                ),
                        )],
                    ),
            )
            .await;
            if confirm {
                act(&mut tx, &placed, "action_confirm").await.unwrap();
            }
        }
        let env = tx.env().with_company(world.company).unwrap();
        let customer = env
            .records("res.partner")
            .unwrap()
            .browse([world.customer])
            .unwrap();
        assert_eq!(
            customer.cell(&mut tx, "sale_order_count").await.unwrap(),
            Cell::Int(2)
        );
        let action: Value = customer
            .call(&mut tx, "action_view_sale_orders", ())
            .await
            .unwrap();
        assert_eq!(
            action_domain(&action),
            Value::list([
                Value::str("&"),
                leaf("partner_id", "=", Value::Int(world.customer)),
                leaf("state", "=", Value::str("sale")),
            ])
        );
        let desk = env
            .records("product.template")
            .unwrap()
            .browse([template])
            .unwrap();
        assert_eq!(
            desk.cell(&mut tx, "sales_order_count").await.unwrap(),
            Cell::Int(2)
        );
        let action: Value = desk.call(&mut tx, "action_view_sales", ()).await.unwrap();
        assert_eq!(
            action_domain(&action),
            Value::list([
                Value::str("&"),
                leaf(
                    "order_line.product_id.product_tmpl_id",
                    "=",
                    Value::Int(template)
                ),
                leaf("state", "=", Value::str("sale")),
            ])
        );
        tx.commit().await.unwrap();
        drop(tx);

        let policy = policy_orm(&db, &orm).await;
        let mut tx = policy
            .begin(erp_orm::UserId::new(seller).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let customer = env
            .records("res.partner")
            .unwrap()
            .browse([world.customer])
            .unwrap();
        assert_eq!(
            customer.cell(&mut tx, "sale_order_count").await.unwrap(),
            Cell::Int(0)
        );
        let desk = env
            .records("product.template")
            .unwrap()
            .browse([template])
            .unwrap();
        assert_eq!(
            desk.cell(&mut tx, "sales_order_count").await.unwrap(),
            Cell::Int(0)
        );
        tx.rollback().await.unwrap();
        drop(tx);
        drop(policy);
        db.cleanup().await.unwrap();
    });
}
