// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Sales Report and the Purchase Report providers: they read as the caller (record rules and
//! column groups), report amounts in the company currency, check every line id and drill target,
//! page the orders of a partner, and agree with "Purchases by Product" on the orders they count.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;
use std::str::FromStr;

use erp_addon_reporting::runtime;
use erp_fields::{Cell, DateTime, Decimal, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Env, Orm, RawVals, SUPERUSER_ID, Transaction, UserId};
use erp_sdk::{Domain, Load, Value};
use erp_testkit::{TemplateDb, TestDb};
use serde_json::{Value as Json, json};

fn system_for_pool(pool: erp_testkit::deadpool_postgres::Pool) -> ModuleSystem {
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
    ModuleSystem::builder(pool, index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
        .require_sql_uninstall_guards([&erp_addon_account::reconcile::history_guard::UNINSTALL])
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_product_mail::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_account_mail::ADDON,
            &erp_addon_reporting::ADDON,
            &erp_addon_sales_team::ADDON,
            &erp_addon_sale::ADDON,
            &erp_addon_purchase::ADDON,
            &erp_addon_purchase_mail::ADDON,
        ])
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

async fn installed() -> (TestDb, Orm) {
    let db = TemplateDb::clone_or_init("erp-suite-sale:report-providers:v1", |pool| async move {
        let system = system_for_pool(pool);
        let initialized = Box::pin(system.initialize(InitOptions::new()))
            .await
            .map_err(|e| e.to_string())?;
        if !initialized.post_commit.is_clean() {
            return Err(format!("initialize: {:?}", initialized.post_commit));
        }
        let modules = ["sale", "purchase"].map(|name| ModuleName::new(name).unwrap());
        let installed =
            Box::pin(system.button_immediate_install(&system.system_env(Context::new()), &modules))
                .await
                .map_err(|e| e.to_string())?;
        if !installed.post_commit.is_clean() {
            return Err(format!("install: {:?}", installed.post_commit));
        }
        Ok::<(), String>(())
    })
    .await
    .unwrap();
    let loaded = Box::pin(system_for_pool(db.pool().clone()).load())
        .await
        .unwrap();
    (db, loaded.orm)
}

/// The installed ORM with the database's compiled access policy, built after the fixture commit.
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

fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap()
}

fn at(text: &str) -> Cell {
    Cell::DateTime(DateTime::parse_odoo(text).unwrap())
}

async fn create(tx: &mut Transaction, company: i64, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .with_company(company)
        .unwrap()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn user(tx: &mut Transaction, company: i64, login: &str, group: (&str, &str)) -> i64 {
    let group = xmlid(tx, group.0, group.1).await;
    let partner = create(
        tx,
        company,
        "res.partner",
        RawVals::new().set_cell("name", Cell::Str(login.into())),
    )
    .await;
    create(
        tx,
        company,
        "res.users",
        RawVals::new()
            .set_cell("name", Cell::Str(login.into()))
            .set_cell("login", Cell::Str(login.into()))
            .set_cell("partner_id", Cell::Id(partner))
            .set_cell("company_id", Cell::Id(company))
            .set_commands("company_ids", [Command::set([id(company)])])
            .set_commands("group_ids", [Command::set([id(group)])]),
    )
    .await
}

struct Fixture {
    db: TestDb,
    orm: Orm,
    /// The ORM without the compiled policy, for fixture writes with the framework's rights.
    installed: Orm,
    company: i64,
    seller: i64,
    buyer: i64,
    /// The seller's customer: one own and one unassigned order.
    azure: i64,
    /// Another seller's customer.
    birch: i64,
    o_own: i64,
    o_none: i64,
    o_other: i64,
    vendor: i64,
    bolt: i64,
    /// Confirmed, expected 2026-03-05, nothing received.
    po_late: i64,
    /// Confirmed, expected 2099.
    po_future: i64,
    sales: i64,
    purchases: i64,
    purchases_by_product: i64,
}

async fn sale_order(
    tx: &mut Transaction,
    company: i64,
    customer: i64,
    seller: Option<i64>,
    product: i64,
    qty: &str,
) -> i64 {
    create(
        tx,
        company,
        "sale.order",
        RawVals::new()
            .set_cell("partner_id", Cell::Id(customer))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("user_id", seller.map_or(Cell::Null, Cell::Id))
            .set_cell("date_order", at("2026-03-10 10:00:00"))
            .set_commands(
                "order_line",
                [Command::create(
                    RawVals::new()
                        .set_cell("product_id", Cell::Id(product))
                        .set_cell("product_uom_qty", Cell::Decimal(dec(qty)))
                        .set_cell("price_unit", Cell::Decimal(dec("100")))
                        .set_cell("purchase_price", Cell::Decimal(dec("60"))),
                )],
            ),
    )
    .await
}

async fn purchase_order(
    tx: &mut Transaction,
    company: i64,
    vendor: i64,
    product: i64,
    qty: &str,
    planned: &str,
) -> i64 {
    create(
        tx,
        company,
        "purchase.order",
        RawVals::new()
            .set_cell("partner_id", Cell::Id(vendor))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("date_order", at("2026-03-10 09:00:00"))
            .set_commands(
                "order_line",
                [Command::create(
                    RawVals::new()
                        .set_cell("product_id", Cell::Id(product))
                        .set_cell("name", Cell::Str("Bolt".into()))
                        .set_cell("product_qty", Cell::Decimal(dec(qty)))
                        .set_cell("price_unit", Cell::Decimal(dec("10")))
                        .set_cell("date_planned", at(planned)),
                )],
            ),
    )
    .await
}

async fn call(tx: &mut Transaction, model: &str, record: i64, method: &'static str) {
    let records = tx.env().records(model).unwrap().browse([record]).unwrap();
    records.call::<(), ()>(tx, method, ()).await.unwrap();
}

async fn setup() -> Fixture {
    let (db, installed) = installed().await;
    let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
    let company = xmlid(&mut tx, "base", "main_company").await;
    // the base rule refuses a change of users while the database has no administrator
    let system = xmlid(&mut tx, "base", "group_system").await;
    let internal = xmlid(&mut tx, "base", "group_user").await;
    create(
        &mut tx,
        company,
        "res.users",
        RawVals::new()
            .set_cell("name", Cell::Str("Report administrator".into()))
            .set_cell("login", Cell::Str("report-admin".into()))
            .set_cell("company_id", Cell::Id(company))
            .set_commands("company_ids", [Command::set([id(company)])])
            .set_commands("group_ids", [Command::set([id(system), id(internal)])]),
    )
    .await;
    let seller = user(
        &mut tx,
        company,
        "report-seller",
        ("sales_team", "group_sale_salesman"),
    )
    .await;
    let other = user(
        &mut tx,
        company,
        "report-other-seller",
        ("sales_team", "group_sale_salesman"),
    )
    .await;
    let buyer = user(
        &mut tx,
        company,
        "report-buyer",
        ("purchase", "group_purchase_user"),
    )
    .await;
    let partner = |name: &str| RawVals::new().set_cell("name", Cell::Str(name.into()));
    let azure = create(&mut tx, company, "res.partner", partner("Azure")).await;
    let birch = create(&mut tx, company, "res.partner", partner("Birch")).await;
    let vendor = create(&mut tx, company, "res.partner", partner("Cedar Supply")).await;
    let desk = create(&mut tx, company, "product.product", partner("Report Desk")).await;
    let bolt = create(&mut tx, company, "product.product", partner("Report Bolt")).await;

    let o_own = sale_order(&mut tx, company, azure, Some(seller), desk, "2").await;
    let o_none = sale_order(&mut tx, company, azure, None, desk, "3").await;
    let o_other = sale_order(&mut tx, company, birch, Some(other), desk, "5").await;

    let po_late = purchase_order(&mut tx, company, vendor, bolt, "5", "2026-03-05 08:00:00").await;
    call(&mut tx, "purchase.order", po_late, "button_confirm").await;
    let po_future =
        purchase_order(&mut tx, company, vendor, bolt, "3", "2099-01-01 08:00:00").await;
    call(&mut tx, "purchase.order", po_future, "button_confirm").await;
    let po_cancel =
        purchase_order(&mut tx, company, vendor, bolt, "7", "2026-03-20 08:00:00").await;
    call(&mut tx, "purchase.order", po_cancel, "button_cancel").await;
    // a request for quotation stays a draft
    purchase_order(&mut tx, company, vendor, bolt, "11", "2026-03-20 08:00:00").await;

    let sales = xmlid(&mut tx, "sale", "report_sales_ledger").await;
    let purchases = xmlid(&mut tx, "purchase", "report_purchase_ledger").await;
    let purchases_by_product = xmlid(&mut tx, "purchase", "report_purchase_by_product").await;
    tx.commit().await.unwrap();
    drop(tx);
    let orm = policy_orm(&db, &installed).await;
    Fixture {
        db,
        orm,
        installed,
        company,
        seller,
        buyer,
        azure,
        birch,
        o_own,
        o_none,
        o_other,
        vendor,
        bolt,
        po_late,
        po_future,
        sales,
        purchases,
        purchases_by_product,
    }
}

async fn as_user(f: &Fixture, user: i64) -> (Transaction, Env) {
    let tx = f.orm.begin(UserId::new(user).unwrap()).await.unwrap();
    let env = tx.env().with_company(f.company).unwrap();
    (tx, env)
}

async fn options(f: &Fixture, env: &Env, tx: &mut Transaction, report: i64) -> Json {
    let input = json!({"report": report, "companies": [f.company], "unfold_all": true,
        "date": {"preset": "custom", "from": "2026-03-01", "to": "2026-03-31"}});
    runtime::report_options(env, tx, report, &input)
        .await
        .unwrap()["options"]
        .clone()
}

fn column(options: &Json, label: &str) -> usize {
    options["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["label"] == label)
        .unwrap_or_else(|| panic!("no column {label}"))
}

fn has_column(options: &Json, label: &str) -> bool {
    options["columns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["label"] == label)
}

fn amount(line: &Json, column: usize) -> Decimal {
    dec(line["cells"][column]["value"].as_str().unwrap())
}

fn lines(answer: &Json) -> Vec<Json> {
    answer["lines"].as_array().unwrap().clone()
}

fn ends_with(lines: &[Json], suffix: &str) -> Option<Json> {
    lines
        .iter()
        .find(|l| l["id"].as_str().unwrap().ends_with(suffix))
        .cloned()
}

fn order_suffix(model: &str, order: i64) -> String {
    format!("group:order_id~{model}~{order}")
}

fn partner_suffix(partner: i64) -> String {
    format!("group:partner_id~res.partner~{partner}")
}

fn refused(result: erp_fields::Result<Json>, message: &str) {
    let error = result.expect_err(message);
    assert!(
        error.message().contains(message),
        "expected {message:?} in {:?}",
        error.message()
    );
}

/// Field `field` of `ids`, summed, read with the framework's rights.
async fn orm_sum(tx: &mut Transaction, model: &str, ids: &[i64], field: &str) -> Decimal {
    let rows = tx
        .env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
        .read_with(tx, &[field], Load::Raw)
        .await
        .unwrap();
    rows.iter()
        .map(|r| match r[field] {
            Value::Decimal(n) => n,
            ref other => panic!("{field} is {other:?}"),
        })
        .sum()
}

async fn domain_ids(env: &Env, tx: &mut Transaction, model: &str, domain: &Json) -> Vec<i64> {
    let mut ids = env
        .records(model)
        .unwrap()
        .search(tx, &Domain::from_json(domain).unwrap())
        .await
        .unwrap()
        .ids();
    ids.sort_unstable();
    ids
}

/// The own-documents salesman: his and the unassigned orders, in the lines, the expansion and the
/// drill-downs; another seller's order and customer stay out.
#[test]
fn the_own_documents_salesman_reads_own_and_unassigned_orders_only() {
    run(async {
        let f = setup().await;
        let (expected_total, expected_margin) = {
            let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
            let visible = [f.o_own, f.o_none];
            let sums = (
                orm_sum(&mut tx, "sale.order", &visible, "amount_total").await,
                orm_sum(&mut tx, "sale.order", &visible, "margin").await,
            );
            tx.rollback().await.unwrap();
            sums
        };
        let (mut tx, env) = as_user(&f, f.seller).await;
        let opts = options(&f, &env, &mut tx, f.sales).await;
        let answer = runtime::report_lines(&env, &mut tx, &opts).await.unwrap();
        let rows = lines(&answer);
        let (total, margin, qty) = (
            column(&opts, "total"),
            column(&opts, "margin"),
            column(&opts, "qty"),
        );
        assert!(ends_with(&rows, &partner_suffix(f.birch)).is_none());
        assert!(ends_with(&rows, &order_suffix("sale.order", f.o_other)).is_none());
        for order in [f.o_own, f.o_none] {
            assert!(ends_with(&rows, &order_suffix("sale.order", order)).is_some());
        }
        let grand = rows.last().unwrap();
        assert_eq!(grand["kind"], "total");
        assert_eq!(amount(grand, total), expected_total);
        assert_eq!(amount(grand, margin), expected_margin);
        assert_eq!(amount(grand, qty), dec("5"));

        let azure = ends_with(&rows, &partner_suffix(f.azure)).unwrap();
        let azure_id = azure["id"].as_str().unwrap();
        let children = lines(
            &runtime::report_expand(&env, &mut tx, &opts, azure_id, 0)
                .await
                .unwrap(),
        );
        assert_eq!(children.len(), 2);
        // a forged line of another seller's customer expands to nothing of his
        let birch_id = azure_id.replace(&partner_suffix(f.azure), &partner_suffix(f.birch));
        let birch = runtime::report_expand(&env, &mut tx, &opts, &birch_id, 0)
            .await
            .unwrap();
        assert!(lines(&birch).is_empty());
        // the drill of a cell lists exactly the orders behind it
        let action = runtime::report_drill(&env, &mut tx, &opts, azure_id, &json!({"cell": total}))
            .await
            .unwrap();
        assert_eq!(
            domain_ids(&env, &mut tx, "sale.order", &action["domain"]).await,
            {
                let mut ids = vec![f.o_own, f.o_none];
                ids.sort_unstable();
                ids
            }
        );
        let forged = format!("{birch_id}|{}", order_suffix("sale.order", f.o_other));
        refused(
            runtime::report_drill(&env, &mut tx, &opts, &forged, &json!({"item": "open"})).await,
            "This order is not in the Sales Report scope.",
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

/// A margin column of a group the caller is not in does not exist for him: no header, no values.
#[test]
fn margin_columns_follow_their_group() {
    run(async {
        let f = setup().await;
        let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute(
            "UPDATE reporting_report_column SET groups = 'sales_team.group_sale_manager' \
             WHERE report_id = $1 AND label IN ('margin', 'margin_pct')",
            &[&f.sales],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let (mut tx, env) = as_user(&f, f.seller).await;
        let opts = options(&f, &env, &mut tx, f.sales).await;
        assert!(!has_column(&opts, "margin") && !has_column(&opts, "margin_pct"));
        let answer = runtime::report_lines(&env, &mut tx, &opts).await.unwrap();
        let width = opts["columns"].as_array().unwrap().len();
        for line in lines(&answer) {
            assert_eq!(line["cells"].as_array().unwrap().len(), width);
        }
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

/// Sales converts each order at its recorded rate; Purchase retains its company-currency restriction.
#[test]
fn sales_converts_foreign_orders_while_purchase_rejects_them() {
    run(async {
        let f = setup().await;
        let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
        let foreign = tx
            .query(
                "SELECT c.id::int8 FROM res_currency c, res_company k \
                 WHERE k.id = $1 AND c.id <> k.currency_id ORDER BY c.id LIMIT 1",
                &[&f.company],
            )
            .await
            .unwrap();
        let foreign: i64 = foreign.first().expect("a second currency").get(0);
        tx.execute(
            "UPDATE sale_order SET currency_id=$1, currency_rate=0.80 WHERE id=$2",
            &[&foreign, &f.o_own],
        )
        .await
        .unwrap();
        tx.execute(
            "UPDATE purchase_order SET currency_id=$1 WHERE id=$2",
            &[&foreign, &f.po_late],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let (mut tx, env) = as_user(&f, f.seller).await;
        let opts = options(&f, &env, &mut tx, f.sales).await;
        let top = lines(&runtime::report_lines(&env, &mut tx, &opts).await.unwrap());
        let customer = ends_with(&top, &partner_suffix(f.azure)).unwrap();
        let detail = lines(
            &runtime::report_expand(&env, &mut tx, &opts, customer["id"].as_str().unwrap(), 0)
                .await
                .unwrap(),
        );
        let fx = ends_with(&detail, &order_suffix("sale.order", f.o_own)).unwrap();
        for (col, field) in [
            ("untaxed", "amount_untaxed"),
            ("total", "amount_total"),
            ("margin", "margin"),
        ] {
            let index = column(&opts, col);
            let converted =
                (orm_sum(&mut tx, "sale.order", &[f.o_own], field).await / dec("0.80")).round_dp(2);
            let domestic = orm_sum(&mut tx, "sale.order", &[f.o_none], field).await;
            assert_eq!(amount(&fx, index), converted);
            assert_eq!(amount(&customer, index), domestic + converted);
            assert_eq!(amount(top.last().unwrap(), index), domestic + converted);
        }
        tx.rollback().await.unwrap();
        drop(tx);
        let (mut tx, env) = as_user(&f, f.buyer).await;
        let opts = options(&f, &env, &mut tx, f.purchases).await;
        refused(
            runtime::report_lines(&env, &mut tx, &opts).await,
            "The Purchase Report has orders in another currency than the company's",
        );
        tx.rollback().await.unwrap();
        f.db.cleanup().await.unwrap();
    });
}

/// Unknown options, malformed or foreign line ids, unknown caret items and cells are refused.
#[test]
fn unknown_options_lines_and_targets_are_refused() {
    run(async {
        let f = setup().await;
        let (mut tx, env) = as_user(&f, f.seller).await;
        let opts = options(&f, &env, &mut tx, f.sales).await;
        let mut bad = opts.clone();
        bad["provider"] = json!({"currency": "USD"});
        refused(
            runtime::report_lines(&env, &mut tx, &bad).await,
            "The Sales Report has no option currency.",
        );
        let rows = lines(&runtime::report_lines(&env, &mut tx, &opts).await.unwrap());
        let azure = ends_with(&rows, &partner_suffix(f.azure)).unwrap();
        let azure_id = azure["id"].as_str().unwrap().to_owned();
        let malformed =
            azure_id.replace(&partner_suffix(f.azure), "group:partner_id~res.partner~x1");
        refused(
            runtime::report_expand(&env, &mut tx, &opts, &malformed, 0).await,
            "This is not a Sales Report line.",
        );
        let wrong_model = azure_id.replace("res.partner", "res.users");
        refused(
            runtime::report_drill(&env, &mut tx, &opts, &wrong_model, &json!({"item": "open"}))
                .await,
            "This is not a Sales Report line.",
        );
        let foreign = azure_id.replace(
            &format!("root~reporting.report~{}", f.sales),
            &format!("root~reporting.report~{}", f.purchases),
        );
        refused(
            runtime::report_expand(&env, &mut tx, &opts, &foreign, 0).await,
            "This line is not part of this Sales Report.",
        );
        refused(
            runtime::report_drill(&env, &mut tx, &opts, &azure_id, &json!({"item": "delete"}))
                .await,
            "This Sales Report line has no action delete.",
        );
        let width = opts["columns"].as_array().unwrap().len();
        refused(
            runtime::report_drill(&env, &mut tx, &opts, &azure_id, &json!({"cell": width})).await,
            "The Sales Report has no such column.",
        );
        refused(
            runtime::report_drill(
                &env,
                &mut tx,
                &opts,
                &azure_id,
                &json!({"cell": column(&opts, "status")}),
            )
            .await,
            "This Sales Report cell has no amount to drill.",
        );
        let total = rows.last().unwrap()["id"].as_str().unwrap().to_owned();
        refused(
            runtime::report_drill(&env, &mut tx, &opts, &total, &json!({"item": "open"})).await,
            "This is not a Sales Report line.",
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

/// With ten orders per page (the smallest page the settings allow), a customer with twelve
/// visible orders gets a first page that ends with a load-more line naming the next offset, and
/// that offset gives the remaining orders.
#[test]
fn orders_of_a_customer_are_paged() {
    run(async {
        let f = setup().await;
        let mut tx = f.installed.begin(SUPERUSER_ID).await.unwrap();
        for _ in 0..10 {
            sale_order(&mut tx, f.company, f.azure, None, f.bolt, "1").await;
        }
        tx.execute(
            "UPDATE reporting_report SET load_more_limit = 10 WHERE id = $1",
            &[&f.sales],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let (mut tx, env) = as_user(&f, f.seller).await;
        let visible = domain_ids(
            &env,
            &mut tx,
            "sale.order",
            &json!([["partner_id", "=", f.azure]]),
        )
        .await;
        assert_eq!(visible.len(), 12);
        let opts = options(&f, &env, &mut tx, f.sales).await;
        let rows = lines(&runtime::report_lines(&env, &mut tx, &opts).await.unwrap());
        let azure_id = ends_with(&rows, &partner_suffix(f.azure)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = lines(
            &runtime::report_expand(&env, &mut tx, &opts, &azure_id, 0)
                .await
                .unwrap(),
        );
        let more = first.iter().find(|l| l["kind"] == "more").unwrap();
        assert_eq!(more["remaining"], 2);
        let next = more["next_offset"].as_i64().unwrap();
        let second = lines(
            &runtime::report_expand(&env, &mut tx, &opts, &azure_id, next)
                .await
                .unwrap(),
        );
        assert!(second.iter().all(|l| l["kind"] != "more"));
        let order_of = |l: &Json| -> i64 {
            l["id"]
                .as_str()
                .unwrap()
                .rsplit('~')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        let page = |ls: &[Json]| -> Vec<i64> {
            ls.iter()
                .filter(|l| l["kind"] == "detail")
                .map(order_of)
                .collect()
        };
        assert_eq!(page(&first).len(), 10);
        assert_eq!(page(&second).len(), 2);
        let mut seen: Vec<i64> = page(&first).into_iter().chain(page(&second)).collect();
        seen.sort_unstable();
        assert_eq!(seen, visible);
        // the first answer already holds the first page and its load-more line
        assert!(rows.iter().any(|l| l["id"] == more["id"]));
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

/// The Purchase Report and "Purchases by Product" count the same confirmed orders: the cancelled
/// order and the request for quotation are in neither.
#[test]
fn purchase_reports_agree_on_confirmed_orders() {
    run(async {
        let f = setup().await;
        let expected = {
            let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
            let lines = domain_ids(
                &tx.env(),
                &mut tx,
                "purchase.order.line",
                &json!([["order_id", "in", [f.po_late, f.po_future]]]),
            )
            .await;
            let sum = orm_sum(&mut tx, "purchase.order.line", &lines, "product_qty").await;
            tx.rollback().await.unwrap();
            sum
        };
        let (mut tx, env) = as_user(&f, f.buyer).await;
        let opts = options(&f, &env, &mut tx, f.purchases).await;
        let rows = lines(&runtime::report_lines(&env, &mut tx, &opts).await.unwrap());
        let vendor = ends_with(&rows, &partner_suffix(f.vendor)).unwrap();
        assert_eq!(amount(&vendor, column(&opts, "ordered")), expected);

        let by_product = options(&f, &env, &mut tx, f.purchases_by_product).await;
        let product_rows = lines(
            &runtime::report_lines(&env, &mut tx, &by_product)
                .await
                .unwrap(),
        );
        let bolt = product_rows
            .iter()
            .find(|l| {
                let id = l["id"].as_str().unwrap();
                id.ends_with(&format!("group:product_id~product.product~{}", f.bolt))
            })
            .unwrap();
        assert_eq!(amount(bolt, column(&by_product, "ordered")), expected);
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

/// An expected arrival before today with goods still to receive carries the danger tone; one in
/// the future does not.
#[test]
fn a_late_arrival_is_marked_danger() {
    run(async {
        let f = setup().await;
        let (mut tx, env) = as_user(&f, f.buyer).await;
        let opts = options(&f, &env, &mut tx, f.purchases).await;
        let rows = lines(&runtime::report_lines(&env, &mut tx, &opts).await.unwrap());
        let planned = column(&opts, "date_planned");
        let late = ends_with(&rows, &order_suffix("purchase.order", f.po_late)).unwrap();
        let future = ends_with(&rows, &order_suffix("purchase.order", f.po_future)).unwrap();
        assert_eq!(late["cells"][planned]["tone"], "danger");
        assert!(future["cells"][planned].get("tone").is_none());
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}
