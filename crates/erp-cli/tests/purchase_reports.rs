// SPDX-License-Identifier: MIT OR Apache-2.0
//! The printed documents of the `purchase` addon (`addons/purchase/views/purchase_reports.xml`): the
//! request for quotation and the purchase order are printed through the module system on a real
//! database and the PDF is read back with an independent parser; a purchase user prints them too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use erp_addon_base::report_print;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_orm::{Command, Context, Orm, SUPERUSER_ID};
use erp_report::LayoutEngine;
use erp_sdk::Decimal;
use erp_sdk::prelude::*;
use erp_testkit::TestDb;

#[path = "support/report_access.rs"]
mod report_access;
#[path = "support/report_frame.rs"]
mod report_frame;

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

/// A fresh database with `purchase` installed.
async fn install(db: &TestDb) -> Orm {
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
            &erp_addon_reporting::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_purchase::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let installed =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("purchase").unwrap()]))
            .await
            .unwrap();
    installed.orm
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn dec(value: &str) -> Decimal {
    value.parse().unwrap()
}

async fn id_of(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module = $1 AND name = $2",
        &[&module, &name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// Text of every page of a PDF without white space.
fn page_texts(bytes: &[u8]) -> Vec<String> {
    let pdf = lopdf::Document::load_mem(bytes).unwrap();
    pdf.get_pages()
        .keys()
        .map(|page| {
            pdf.extract_text(&[*page])
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        })
        .collect()
}

/// Print `orders` through the report `key` and render the PDF; the file is kept under the test's
/// temporary directory as `name` for a look at the page.
async fn print(orm: &Orm, key: &str, orders: &[i64], name: &str) -> Vec<String> {
    let main = {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = id_of(&mut tx, "base", "main_company").await;
        tx.rollback().await.unwrap();
        main
    };
    let mut tx = orm
        .begin_with(SUPERUSER_ID, Context::new().with_company_id(main))
        .await
        .unwrap();
    let report = report_print::find(&mut tx, key).await.unwrap();
    let document = report_print::prepare(&mut tx, &report, orders)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let engine = erp_report_typst::TypstEngine::new(
        addons_dir().join("report_fonts"),
        erp_report::Limits::default(),
    )
    .unwrap();
    let pdf = engine
        .render(&report_frame::through_frame(&document))
        .unwrap();
    fs::write(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(name),
        pdf.bytes(),
    )
    .unwrap();
    page_texts(pdf.bytes())
}

/// The request for quotation and the purchase order of a short order and of a 60-line order print
/// from the Print menu's reports: title, number, vendor and delivery address, the lines, and, on the
/// purchase order only, prices, taxes and totals; the lines table repeats its header on every page.
#[test]
fn rfq_and_purchase_order_print() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let main = id_of(&mut tx, "base", "main_company").await;
        let unit = id_of(&mut tx, "uom", "product_uom_unit").await;
        let us: i64 = tx
            .query("SELECT id::int8 FROM res_country WHERE code = 'US'", &[])
            .await
            .unwrap()[0]
            .get(0);
        // the delivery address is the company's own: give it one
        tx.query(
            "UPDATE res_partner SET street = 'Jalan Merdeka 1', zip = '10110', city = 'Jakarta' \
             WHERE id = (SELECT partner_id FROM res_company WHERE id = $1)",
            &[&main],
        )
        .await
        .unwrap();
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
            RawVals::new()
                .set_cell("name", text("Sinar Abadi"))
                .set_cell("street", text("Jalan Asia Afrika 8"))
                .set_cell("zip", text("40112"))
                .set_cell("city", text("Bandung")),
        )
        .await;
        let group = create(
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
                .set_cell("tax_group_id", Cell::Id(group)),
        )
        .await;
        let bolt = create(
            &mut tx,
            "product.product",
            RawVals::new()
                .set_cell("name", text("Bolt"))
                .set_cell("standard_price", Cell::Decimal(dec("2.50")))
                .set_cell("uom_id", Cell::Id(unit)),
        )
        .await;
        let line = |name: &str, qty: &str, discount: &str| {
            Command::Create(
                RawVals::new()
                    .set_cell("product_id", Cell::Id(bolt))
                    .set_cell("name", text(name))
                    .set_cell("product_qty", Cell::Decimal(dec(qty)))
                    .set_cell("price_unit", Cell::Decimal(dec("2.50")))
                    .set_cell("discount", Cell::Decimal(dec(discount)))
                    .set_commands(
                        "tax_ids",
                        [Command::Set(vec![RecId::from_real(tax).unwrap()])],
                    ),
            )
        };
        let order = |lines: Vec<Command>| {
            RawVals::new()
                .set_cell("partner_id", Cell::Id(vendor))
                .set_commands("order_line", lines)
        };
        let orders = env.records("purchase.order").unwrap();
        let short = orders
            .create(
                &mut tx,
                vec![order(vec![
                    line("Hex bolt M8 zinc plated", "4", "0"),
                    line("Hex bolt M10 zinc plated", "6", "10"),
                ])],
            )
            .await
            .unwrap()
            .ids()[0];
        let long = orders
            .create(
                &mut tx,
                vec![order(
                    (1..=60)
                        .map(|n| {
                            line(
                                &format!(
                                    "Rowstart{n}a a line description that is long enough to wrap onto a second line of the cell before it ends Rowend{n}."
                                ),
                                "1",
                                "0",
                            )
                        })
                        .collect(),
                )],
            )
            .await
            .unwrap()
            .ids()[0];
        // twenty lines in rupiah, amounts in the millions, over two pages
        tx.query(
            "UPDATE res_currency SET active = true WHERE name = 'IDR'",
            &[],
        )
        .await
        .unwrap();
        let idr: i64 = tx
            .query("SELECT id::int8 FROM res_currency WHERE name = 'IDR'", &[])
            .await
            .unwrap()[0]
            .get(0);
        let rate_type: i64 = tx
            .query(
                "SELECT id::int8 FROM res_currency_rate_type ORDER BY id LIMIT 1",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        create(
            &mut tx,
            "res.currency.rate",
            RawVals::new()
                .set_cell("currency_id", Cell::Id(idr))
                .set_cell("name", Cell::Date(Date::from_ymd(2026, 1, 1).unwrap()))
                .set_cell("rate", Cell::Decimal(dec("16000")))
                .set_cell("rate_type_id", Cell::Id(rate_type)),
        )
        .await;
        let big_line = |n: usize| {
            Command::Create(
                RawVals::new()
                    .set_cell("product_id", Cell::Id(bolt))
                    .set_cell(
                        "name",
                        text(&format!(
                            "Bigrow{n}a heavy steel bracket with a long description that wraps onto a second line Bigend{n}."
                        )),
                    )
                    .set_cell("product_qty", Cell::Decimal(dec("12")))
                    .set_cell("price_unit", Cell::Decimal(dec("1250000")))
                    .set_commands(
                        "tax_ids",
                        [Command::Set(vec![RecId::from_real(tax).unwrap()])],
                    ),
            )
        };
        let rupiah = orders
            .create(
                &mut tx,
                vec![
                    order((1..=20).map(big_line).collect()).set_cell("currency_id", Cell::Id(idr)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();

        // the Print menu lists both reports for the order model
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let bound: Vec<String> = tx
            .query(
                "SELECT r.report_name FROM ir_act_report_xml r JOIN ir_model m ON m.id = r.binding_model_id \
                 WHERE m.model = 'purchase.order' AND r.binding_type = 'report' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();
        assert_eq!(bound, ["purchase.report_order", "purchase.report_rfq"]);
        tx.rollback().await.unwrap();

        // request for quotation: no prices, no totals
        let pages = print(&orm, "purchase.report_rfq", &[short], "purchase-rfq.pdf").await;
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        for needle in [
            "RequestforQuotation",
            "Draft",
            "Vendor",
            "SinarAbadi",
            "JalanAsiaAfrika8",
            "40112Bandung",
            "DeliveryAddress",
            "JalanMerdeka1",
            "10110Jakarta",
            "OrderDeadline",
            "Hexbolt M8zincplated".replace(' ', "").as_str(),
            "ExpectedArrival",
            "Quantity",
        ] {
            assert!(page.contains(needle), "{needle}: {page}");
        }
        for absent in ["UnitPrice", "UntaxedAmount", "10%"] {
            assert!(!page.contains(absent), "{absent} on the RFQ: {page}");
        }

        // purchase order: prices, taxes, totals, signatures
        let pages = print(
            &orm,
            "purchase.report_order",
            &[short],
            "purchase-order.pdf",
        )
        .await;
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        for needle in [
            "PurchaseOrder",
            "SinarAbadi",
            "UnitPrice",
            "Disc.%",
            "10%",
            "UntaxedAmount",
            "Total",
            "AuthorisedBy".to_lowercase().as_str(),
        ] {
            let found =
                page.contains(needle) || page.to_lowercase().contains(&needle.to_lowercase());
            assert!(found, "{needle}: {page}");
        }

        // 60 lines: the header repeats on every page, the closing blocks print once at the end
        let pages = print(
            &orm,
            "purchase.report_order",
            &[long],
            "purchase-order-long.pdf",
        )
        .await;
        assert!(pages.len() >= 2, "60 lines fill more than one page");
        let last = pages.len() - 1;
        for (index, page) in pages.iter().enumerate() {
            if page.contains("Rowstart") {
                assert!(page.contains("DescriptionArrival"), "page {index}: {page}");
            }
            assert_eq!(
                page.contains("UntaxedAmount"),
                index == last,
                "page {index}"
            );
        }
        for n in 1..=60 {
            let page = pages
                .iter()
                .position(|p| p.contains(&format!("Rowstart{n}a")))
                .unwrap_or_else(|| panic!("line {n} is printed"));
            assert!(
                pages[page].contains(&format!("Rowend{n}.")),
                "line {n} split"
            );
        }
        assert!(
            !pages.iter().any(|p| p.contains("Disc.%")),
            "no line has a discount"
        );
        let pages = print(
            &orm,
            "purchase.report_order",
            &[rupiah],
            "purchase-order-rupiah.pdf",
        )
        .await;
        assert!(pages.len() >= 2, "twenty wrapped lines fill two pages");
        let pages = print(
            &orm,
            "purchase.report_rfq",
            &[long],
            "purchase-rfq-long.pdf",
        )
        .await;
        assert!(pages.len() >= 2);

        // A purchase user of the main company prints both documents; a user without a purchase
        // role and an order of another company are refused.
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let currency: i64 = tx
            .query(
                "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                &[&main],
            )
            .await
            .unwrap()[0]
            .get(0);
        let second = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Harbour Supplies"))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let elsewhere = tx
            .env()
            .with_company(second)
            .unwrap()
            .records("purchase.order")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(vendor))
                        .set_cell("company_id", Cell::Id(second)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        report_access::admin(&mut tx, main).await;
        let buyer = report_access::user(
            &mut tx,
            "report-buyer",
            main,
            &[("purchase", "group_purchase_user")],
        )
        .await;
        let outsider = report_access::user(&mut tx, "report-purchase-outsider", main, &[]).await;
        // the order's buyer is a colleague of the user who prints it
        let colleague = report_access::user(
            &mut tx,
            "report-buyer-colleague",
            main,
            &[("purchase", "group_purchase_user")],
        )
        .await;
        tx.query(
            "UPDATE purchase_order SET user_id = $1 WHERE id = $2",
            &[&colleague, &short],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let users = report_access::policy_orm(&db, &orm).await;
        for (report, title) in [
            ("purchase.report_rfq", "RequestforQuotation"),
            ("purchase.report_order", "PurchaseOrder"),
        ] {
            let pages = report_access::print_as(&users, buyer, main, report, &[short])
                .await
                .unwrap_or_else(|e| panic!("{report} as a purchase user: {e:?}"));
            for needle in [title, "SinarAbadi", "report-buyer-colleague"] {
                assert!(pages[0].contains(needle), "{needle}: {}", pages[0]);
            }
            report_access::refused(
                report_access::print_as(&users, outsider, main, report, &[short]).await,
                &format!("{report} as a user without a purchase role"),
            );
            report_access::refused(
                report_access::print_as(&users, buyer, main, report, &[elsewhere]).await,
                &format!("{report} of another company"),
            );
        }
        drop(users);
        db.cleanup().await.unwrap();
    });
}
