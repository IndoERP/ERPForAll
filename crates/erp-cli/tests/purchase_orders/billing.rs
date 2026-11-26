// SPDX-License-Identifier: MIT OR Apache-2.0
//! The billed quantity, the quantity to bill and the billing status of purchase orders, the bill
//! control of products and its Purchase setting, the filters and the upgrade of existing orders,
//! and what an edited quantity, a changed bill control and a cancelled order do to them.

use super::*;
use erp_addon_purchase::settings::BILL_CONTROL_SETTING;

/// The billed quantity and the quantity to bill of one line.
async fn billed(tx: &mut Transaction, line: &Records) -> (Decimal, Decimal) {
    (
        num(tx, line, "qty_invoiced").await,
        num(tx, line, "qty_to_invoice").await,
    )
}

async fn status(tx: &mut Transaction, order: &Records) -> String {
    match cell(tx, order, "invoice_status").await {
        Cell::Str(status) => status.to_string(),
        other => panic!("invoice_status is {other:?}"),
    }
}

async fn bill(tx: &mut Transaction, order: &Records, method: &'static str) -> i64 {
    let Value::Json(action) = order.call::<(), Value>(tx, method, ()).await.unwrap() else {
        panic!("expected the vendor bill form")
    };
    assert_eq!(action["res_model"], "account.move");
    action["res_id"].as_i64().unwrap()
}

/// The bill item of `bill` made from the purchase line `line`.
async fn item_of(tx: &mut Transaction, company: i64, bill: i64, line: &Records) -> Records {
    let id: i64 = tx
        .query(
            "SELECT bill_line_id FROM purchase_direct_bill_line WHERE bill_id=$1 AND purchase_line_id=$2",
            &[&bill, &line.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.move.line")
        .unwrap()
        .browse([id])
        .unwrap()
}

async fn set_quantity(tx: &mut Transaction, item: &Records, quantity: &str) {
    item.write(
        tx,
        RawVals::new().set_cell("quantity", Cell::Decimal(dec(quantity))),
    )
    .await
    .unwrap();
}

fn d(value: &str) -> (Decimal, Decimal) {
    let (billed, to_bill) = value.split_once('/').unwrap();
    (dec(billed).normalize(), dec(to_bill).normalize())
}

#[test]
fn bill_control_drives_billed_quantities_the_status_and_the_bill_buttons() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_modules(&db, &["purchase_stock"]).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        user(&mut tx, "billing-admin", &[system], &[company]).await;
        let purchase = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let accounting = xmlid(&mut tx, "account", "group_account_user").await;
        let clerk = user(
            &mut tx,
            "billing-clerk",
            &[purchase, accounting],
            &[company],
        )
        .await;
        let buyer = user(&mut tx, "billing-buyer", &[purchase], &[company]).await;
        bill_books::books(&mut tx, company).await;
        let k = catalog(&mut tx, company).await;
        let products = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap();
        let service = products
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Installation"))
                        .set_cell("type", text("service"))
                        .set_cell("uom_id", Cell::Id(k.unit)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        // goods take the setting's default, services bill ordered quantities
        for (product, control) in [(k.bolt, "receive"), (service, "purchase")] {
            let product = products.browse([product]).unwrap();
            assert_eq!(
                cell(&mut tx, &product, "purchase_method").await,
                text(control)
            );
        }
        // a service that becomes goods takes the setting's control, as on every type change
        let repair = products
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Repair"))
                        .set_cell("type", text("service"))
                        .set_cell("uom_id", Cell::Id(k.unit)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            cell(&mut tx, &repair, "purchase_method").await,
            text("purchase")
        );
        write(
            &mut tx,
            &repair,
            RawVals::new().set_cell("type", text("consu")),
        )
        .await
        .unwrap();
        assert_eq!(
            cell(&mut tx, &repair, "purchase_method").await,
            text("receive")
        );
        // a write that names the type it already has is no type change: a control chosen by hand
        // stays, also when nothing of the product is loaded yet (a fresh client call)
        write(
            &mut tx,
            &repair,
            RawVals::new().set_cell("purchase_method", text("purchase")),
        )
        .await
        .unwrap();
        tx.flush().await.unwrap();
        tx.invalidate_all().await.unwrap();
        write(
            &mut tx,
            &repair,
            RawVals::new().set_cell("type", text("consu")),
        )
        .await
        .unwrap();
        assert_eq!(
            cell(&mut tx, &repair, "purchase_method").await,
            text("purchase")
        );
        let po = order(
            &mut tx,
            &[company],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [
                        Command::Create(
                            line_vals(k.bolt)
                                .set_cell("product_qty", Cell::Decimal(dec("3")))
                                .set_cell("price_unit", Cell::Decimal(dec("10"))),
                        ),
                        Command::Create(
                            line_vals(service)
                                .set_cell("product_qty", Cell::Decimal(dec("2")))
                                .set_cell("price_unit", Cell::Decimal(dec("40"))),
                        ),
                    ],
                ),
        )
        .await;
        let [goods, work] = <[Records; 2]>::try_from(lines_of(&mut tx, &[company], &po).await)
            .unwrap_or_else(|_| panic!("two lines"));
        assert_eq!(status(&mut tx, &po).await, "no");
        assert_eq!(
            billed(&mut tx, &work).await,
            d("0/0"),
            "an RFQ has nothing to bill"
        );
        act(&mut tx, &po, "button_confirm").await.unwrap();
        assert_eq!(
            billed(&mut tx, &goods).await,
            d("0/0"),
            "nothing received yet"
        );
        assert_eq!(
            billed(&mut tx, &work).await,
            d("0/2"),
            "the service bills its order"
        );
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        // receive two of the three bolts, no backorder
        let order_id = po.ids()[0];
        let movement=&tx.query("SELECT m.picking_id,m.id,m.picking_type_id FROM purchase_stock_receipt_move_link l JOIN stock_move m ON m.id=l.move_id JOIN purchase_order_line p ON p.id=l.purchase_line_id WHERE p.order_id=$1", &[&order_id]).await.unwrap()[0];
        let (receipt, mov, kind): (i64, i64, i64) =
            (movement.get(0), movement.get(1), movement.get(2));
        tx.env()
            .records("stock.picking.type")
            .unwrap()
            .browse([kind])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("create_backorder", text("never")),
            )
            .await
            .unwrap();
        let details = tx.env().records("stock.move.line").unwrap();
        let done = RawVals::new().set_cell("quantity", Cell::Decimal(dec("2")));
        let rows = tx
            .query("SELECT id FROM stock_move_line WHERE move_id=$1", &[&mov])
            .await
            .unwrap();
        if let Some(row) = rows.first() {
            details
                .browse([row.get(0)])
                .unwrap()
                .write(&mut tx, done)
                .await
                .unwrap();
        } else {
            details
                .create(&mut tx, vec![done.set_cell("move_id", Cell::Id(mov))])
                .await
                .unwrap();
        }
        returns::validate(&mut tx, receipt).await.unwrap();
        assert_eq!(billed(&mut tx, &goods).await, d("0/2"));
        tx.commit().await.unwrap();
        drop(tx);

        // a buyer without accounting rights reads the quantities and the status, not the bills
        let mut tx = orm.begin(buyer).await.unwrap();
        let mine = orders(&tx, &[company]).browse([order_id]).unwrap();
        assert_eq!(status(&mut tx, &mine).await, "to invoice");
        let line = lines(&tx, &[company]).browse(goods.ids()).unwrap();
        assert_eq!(billed(&mut tx, &line).await, d("0/2"));
        // the bill links are never written by a client, through x2many commands neither
        for command in [Command::Clear, Command::create(RawVals::new())] {
            refused(
                write(
                    &mut tx,
                    &line,
                    RawVals::new().set_commands("direct_bill_line_ids", [command]),
                )
                .await,
                ErrorKind::Validation,
                &["direct_bill_line_ids"],
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let po = orders(&tx, &[company]).browse([order_id]).unwrap();
        let goods = lines(&tx, &[company]).browse(goods.ids()).unwrap();
        let work = lines(&tx, &[company]).browse(work.ids()).unwrap();
        // the first bill takes each line's quantity to bill
        let first = bill(&mut tx, &po, "action_create_vendor_bill").await;
        let goods_item = item_of(&mut tx, company, first, &goods).await;
        let work_item = item_of(&mut tx, company, first, &work).await;
        assert_eq!(num(&mut tx, &goods_item, "quantity").await, dec("2"));
        assert_eq!(num(&mut tx, &work_item, "quantity").await, dec("2"));
        assert_eq!(billed(&mut tx, &goods).await, d("2/0"));
        assert_eq!(billed(&mut tx, &work).await, d("2/0"));
        assert_eq!(status(&mut tx, &po).await, "invoiced");
        // a draft edit in Accounting moves both numbers and the status
        set_quantity(&mut tx, &goods_item, "1.5").await;
        assert_eq!(billed(&mut tx, &goods).await, d("1.5/0.5"));
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        // the remaining half bolt, then a cancelled and a deleted bill free their quantities
        let second = bill(&mut tx, &po, "action_bill_remaining").await;
        assert_ne!(second, first);
        let half = item_of(&mut tx, company, second, &goods).await;
        assert_eq!(num(&mut tx, &half, "quantity").await, dec("0.5"));
        assert_eq!(billed(&mut tx, &goods).await, d("2/0"));
        assert_eq!(status(&mut tx, &po).await, "invoiced");
        bill_books::moves(&tx, company)
            .browse([second])
            .unwrap()
            .call::<(), ()>(&mut tx, "button_cancel", ())
            .await
            .unwrap();
        assert_eq!(
            billed(&mut tx, &goods).await,
            d("1.5/0.5"),
            "a cancelled bill"
        );
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        let third = bill(&mut tx, &po, "action_bill_remaining").await;
        assert_ne!(third, second);
        assert_eq!(billed(&mut tx, &goods).await, d("2/0"));
        bill_books::moves(&tx, company)
            .browse([third])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap();
        assert_eq!(
            billed(&mut tx, &goods).await,
            d("1.5/0.5"),
            "a deleted bill"
        );
        // a removed draft item counts nothing
        let removed = item_of(&mut tx, company, first, &work).await;
        bill_books::moves(&tx, company)
            .browse([first])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "invoice_line_ids",
                    [Command::Delete(RecId::from_real(removed.ids()[0]).unwrap())],
                ),
            )
            .await
            .unwrap();
        assert_eq!(billed(&mut tx, &work).await, d("0/2"), "a removed item");
        // posting keeps the billed quantity
        bill_books::moves(&tx, company)
            .browse([first])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("invoice_date", Cell::Date(bill_books::d(2024, 6, 3)))
                    .set_cell("date", Cell::Date(bill_books::d(2024, 6, 3))),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        bill_books::periods(&orm, company).await;
        let mut tx = orm.begin(clerk).await.unwrap();
        let po = orders(&tx, &[company]).browse([order_id]).unwrap();
        let goods = lines(&tx, &[company]).browse(goods.ids()).unwrap();
        bill_books::moves(&tx, company)
            .browse([first])
            .unwrap()
            .call::<(), ()>(&mut tx, "action_post", ())
            .await
            .unwrap();
        assert_eq!(billed(&mut tx, &goods).await, d("1.5/0.5"), "a posted bill");
        // a fourth bill above the received quantity: the line and the button agree
        let fourth = bill(&mut tx, &po, "action_bill_remaining").await;
        let over = item_of(&mut tx, company, fourth, &goods).await;
        set_quantity(&mut tx, &over, "1.5").await;
        assert_eq!(
            billed(&mut tx, &goods).await,
            d("3/-1"),
            "billed above receipt"
        );
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        let over_billed = po.clone();
        refused(
            tx.savepoint(async move |tx| {
                over_billed
                    .call::<(), Value>(tx, "action_bill_remaining", ())
                    .await
            })
            .await,
            ErrorKind::UserError,
            &["exceed"],
        );
        set_quantity(&mut tx, &over, "0.5").await;
        assert_eq!(billed(&mut tx, &goods).await, d("2/0"));
        // a linked vendor credit note subtracts its quantity
        let credit = bill_books::moves(&tx, company)
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("move_type", text("in_refund"))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("partner_id", Cell::Id(k.vendor))
                        .set_commands(
                            "invoice_line_ids",
                            [Command::Create(
                                RawVals::new()
                                    .set_cell("product_id", Cell::Id(k.bolt))
                                    .set_cell("quantity", Cell::Decimal(dec("0.5")))
                                    .set_cell("price_unit", Cell::Decimal(dec("10"))),
                            )],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let credit_item: i64 = tx
            .query(
                "SELECT id FROM account_move_line WHERE move_id=$1 AND display_type='product'",
                &[&credit],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.env()
            .sudo(true)
            .records("purchase.direct.bill.line")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("order_id", Cell::Id(order_id))
                        .set_cell("bill_id", Cell::Id(credit))
                        .set_cell("purchase_line_id", Cell::Id(goods.ids()[0]))
                        .set_cell("bill_line_id", Cell::Id(credit_item)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(billed(&mut tx, &goods).await, d("1.5/0.5"), "a credit note");
        // the two filters of the order search
        let waiting = orders(&tx, &[company])
            .search(
                &mut tx,
                &Domain::condition("invoice_status", Operator::Eq, "to invoice").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.ids(), [order_id]);
        let full = orders(&tx, &[company])
            .search(
                &mut tx,
                &Domain::condition("invoice_status", Operator::Eq, "invoiced").unwrap(),
            )
            .await
            .unwrap();
        assert!(full.is_empty());
        for (filter, value) in [
            ("waiting_bills", "to invoice"),
            ("fully_billed", "invoiced"),
        ] {
            let found: i64 = tx
                .query(
                    "SELECT count(*) FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id
                     AND d.model = 'ir.ui.view' AND d.module = 'purchase' AND d.name = 'purchase_order_search'
                     WHERE v.arch_db ~ $1 AND v.arch_db ~ $2",
                    &[&format!("name=.{filter}."), &format!("invoice_status.*{value}")],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(found, 1, "the search view filters on {value} as {filter}");
        }
        tx.commit().await.unwrap();
        drop(tx);

        // the buyer still reads the numbers, and still cannot read any bill
        let mut tx = orm.begin(buyer).await.unwrap();
        let line = lines(&tx, &[company]).browse(goods.ids()).unwrap();
        assert_eq!(billed(&mut tx, &line).await, d("1.5/0.5"));
        let denied = bill_books::moves(&tx, company)
            .browse([first])
            .unwrap()
            .cell(&mut tx, "state")
            .await
            .unwrap_err();
        assert_eq!(denied.kind(), ErrorKind::Access, "{denied:?}");
        tx.rollback().await.unwrap();
    })
}

#[test]
fn the_setting_decides_new_products_and_an_upgrade_fills_existing_orders() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let admin = user(&mut tx, "bill-control-admin", &[system], &[company]).await;
        let purchase = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let accounting = xmlid(&mut tx, "account", "group_account_user").await;
        let clerk = user(
            &mut tx,
            "bill-control-clerk",
            &[purchase, accounting],
            &[company],
        )
        .await;
        bill_books::books(&mut tx, company).await;
        let k = catalog(&mut tx, company).await;
        tx.commit().await.unwrap();
        drop(tx);

        // the administrator sets new products to bill ordered quantities
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env().clone();
        let keys = vec![BILL_CONTROL_SETTING.to_owned()];
        let read = read_values(&env, &mut tx, SettingsScope::Global {}, keys)
            .await
            .unwrap();
        let revision = erp_sdk::serde_json::to_value(&read).unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            revision,
            vec![(BILL_CONTROL_SETTING.to_owned(), Value::str("purchase"))],
        )
        .await
        .unwrap();
        // a product created after the change takes the new default
        let gasket = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Gasket"))
                        .set_cell("uom_id", Cell::Id(k.unit)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let products = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap();
        let gasket = products.browse([gasket]).unwrap();
        assert_eq!(
            cell(&mut tx, &gasket, "purchase_method").await,
            text("purchase")
        );
        let bolt = products.browse([k.bolt]).unwrap();
        assert_eq!(
            cell(&mut tx, &bolt, "purchase_method").await,
            text("receive")
        );
        let po = order(
            &mut tx,
            &[company],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [
                        Command::Create(
                            line_vals(gasket.ids()[0])
                                .set_cell("product_qty", Cell::Decimal(dec("4")))
                                .set_cell("price_unit", Cell::Decimal(dec("5"))),
                        ),
                        Command::Create(
                            line_vals(k.bolt).set_cell("product_qty", Cell::Decimal(dec("2"))),
                        ),
                    ],
                ),
        )
        .await;
        let [ordered, received] =
            <[Records; 2]>::try_from(lines_of(&mut tx, &[company], &po).await)
                .unwrap_or_else(|_| panic!("two lines"));
        // the computed fields are never written by a client
        refused(
            write(
                &mut tx,
                &ordered,
                RawVals::new().set_cell("qty_invoiced", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::Validation,
            &["qty_invoiced"],
        );
        refused(
            write(
                &mut tx,
                &po,
                RawVals::new().set_cell("invoice_status", text("invoiced")),
            )
            .await,
            ErrorKind::Validation,
            &["invoice_status"],
        );
        act(&mut tx, &po, "button_confirm").await.unwrap();
        // without receipts: ordered quantities bill at once, received quantities wait
        assert_eq!(billed(&mut tx, &ordered).await, d("0/4"));
        assert_eq!(billed(&mut tx, &received).await, d("0/0"));
        let first = bill(&mut tx, &po, "action_create_vendor_bill").await;
        let rows = tx
            .query(
                "SELECT purchase_line_id FROM purchase_direct_bill_line WHERE bill_id=$1",
                &[&first],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "only the line with a quantity to bill");
        let item = item_of(&mut tx, company, first, &ordered).await;
        assert_eq!(num(&mut tx, &item, "quantity").await, dec("4"));
        assert_eq!(status(&mut tx, &po).await, "invoiced");
        set_quantity(&mut tx, &item, "3").await;
        assert_eq!(billed(&mut tx, &ordered).await, d("3/1"));
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        let order_id = po.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        // a database of the previous release: none of the new columns, purchase at 1.3.0
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute(
            "ALTER TABLE purchase_order_line DROP COLUMN qty_invoiced, DROP COLUMN qty_to_invoice",
            &[],
        )
        .await
        .unwrap();
        tx.execute("ALTER TABLE purchase_order DROP COLUMN invoice_status", &[])
            .await
            .unwrap();
        tx.execute(
            "ALTER TABLE product_template DROP COLUMN purchase_method",
            &[],
        )
        .await
        .unwrap();
        tx.execute(
            "UPDATE ir_module_module SET latest_version='1.3.0' WHERE name='purchase'",
            &[],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        drop(orm);
        let system = purchase_module_system(&db);
        let upgraded = Box::pin(system.button_immediate_upgrade(
            &system.system_env(Context::new()),
            &[ModuleName::new("purchase").unwrap()],
        ))
        .await
        .unwrap();
        assert!(
            upgraded.post_commit.is_clean(),
            "{:?}",
            upgraded.post_commit
        );
        let orm = purchase_policy_orm(&db, &upgraded.orm, &[]).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let empty: i64 = tx
            .query(
                "SELECT (SELECT count(*) FROM purchase_order_line WHERE qty_invoiced IS NULL OR qty_to_invoice IS NULL)
                      + (SELECT count(*) FROM purchase_order WHERE invoice_status IS NULL)
                      + (SELECT count(*) FROM product_template WHERE purchase_method IS NULL)",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(empty, 0, "the upgrade fills every new column");
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(clerk).await.unwrap();
        let po = orders(&tx, &[company]).browse([order_id]).unwrap();
        let ordered = lines(&tx, &[company]).browse(ordered.ids()).unwrap();
        let received = lines(&tx, &[company]).browse(received.ids()).unwrap();
        // products without a stored control take the setting's (now ordered quantities)
        assert_eq!(billed(&mut tx, &ordered).await, d("3/1"));
        assert_eq!(billed(&mut tx, &received).await, d("0/2"));
        assert_eq!(status(&mut tx, &po).await, "to invoice");
        // the ordered quantity edited after a bill exists: the quantity to bill follows
        write(
            &mut tx,
            &ordered,
            RawVals::new().set_cell("product_qty", Cell::Decimal(dec("3.5"))),
        )
        .await
        .unwrap();
        assert_eq!(billed(&mut tx, &ordered).await, d("3/0.5"));
        // a product's bill control changed after confirmation: its line follows
        let bolt = tx
            .env()
            .sudo(true)
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap()
            .browse([k.bolt])
            .unwrap();
        write(
            &mut tx,
            &bolt,
            RawVals::new().set_cell("purchase_method", text("receive")),
        )
        .await
        .unwrap();
        assert_eq!(billed(&mut tx, &received).await, d("0/0"));
        // a cancelled order takes its draft bill along: nothing billed, nothing left to bill
        act(&mut tx, &po, "button_cancel").await.unwrap();
        assert_eq!(billed(&mut tx, &ordered).await, d("0/0"));
        assert_eq!(billed(&mut tx, &received).await, d("0/0"));
        assert_eq!(status(&mut tx, &po).await, "no");
        tx.rollback().await.unwrap();
    })
}

/// Installing `purchase` over existing products fills their bill control before the setting's
/// policy group exists: a first install, with no stored choice, gives the declared default; a
/// reinstall over a stored choice gives that choice.
#[test]
fn installing_purchase_over_products_takes_the_stored_bill_control_or_the_default() {
    for (stored, expected) in [(None, "receive"), (Some("purchase"), "purchase")] {
        run(async move {
            let db = TestDb::builder().pool_size(8).create().await.unwrap();
            let orm = install_module(&db, "product").await;
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let washer = tx
                .env()
                .records("product.template")
                .unwrap()
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("name", text("Washer"))],
                )
                .await
                .unwrap()
                .ids()[0];
            if let Some(choice) = stored {
                tx.execute(
                    "INSERT INTO ir_config_setting_scope(identity, revision) VALUES ('global', '0') \
                     ON CONFLICT (identity) DO NOTHING",
                    &[],
                )
                .await
                .unwrap();
                tx.execute(
                    "INSERT INTO ir_config_setting_value(scope_id, setting_key, value_text) \
                     SELECT id, $1, $2 FROM ir_config_setting_scope WHERE identity = 'global'",
                    &[&BILL_CONTROL_SETTING, &choice],
                )
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
            drop(tx);
            drop(orm);
            let system = purchase_module_system(&db);
            let installed = Box::pin(system.button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("purchase").unwrap()],
            ))
            .await
            .unwrap();
            assert!(
                installed.post_commit.is_clean(),
                "{:?}",
                installed.post_commit
            );
            let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
            let method: String = tx
                .query(
                    "SELECT purchase_method::text FROM product_template WHERE id = $1",
                    &[&washer],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(method, expected, "stored choice {stored:?}");
            tx.rollback().await.unwrap();
            drop(tx);
            db.cleanup().await.unwrap();
        });
    }
}

async fn bill_state(tx: &mut Transaction, bill: i64) -> String {
    tx.query(
        "SELECT state::text FROM account_move WHERE id = $1",
        &[&bill],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// Cancelling a purchase order cancels its draft bills, also for a buyer without accounting
/// rights, so nothing of a cancelled or redrafted order is posted afterwards; a posted bill
/// refuses the cancel without naming a bill the buyer cannot read; the billed quantity is a floor
/// a raise or an unchanged quantity may stay below.
#[test]
fn cancelled_purchases_take_their_draft_bills_along_and_keep_the_billed_floor() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install_modules(&db, &["purchase"]).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        user(&mut tx, "cancel-bills-admin", &[system], &[company]).await;
        let purchase = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let buyer = user(&mut tx, "cancel-bills-buyer", &[purchase], &[company]).await;
        bill_books::books(&mut tx, company).await;
        let k = catalog(&mut tx, company).await;
        let service = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Cleaning"))
                        .set_cell("type", text("service"))
                        .set_cell("uom_id", Cell::Id(k.unit)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut made = Vec::new();
        for _ in 0..3 {
            let po = order(
                &mut tx,
                &[company],
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(k.vendor))
                    .set_commands(
                        "order_line",
                        [Command::Create(
                            line_vals(service)
                                .set_cell("product_qty", Cell::Decimal(dec("2")))
                                .set_cell("price_unit", Cell::Decimal(dec("10"))),
                        )],
                    ),
            )
            .await;
            act(&mut tx, &po, "button_confirm").await.unwrap();
            let bill = bill(&mut tx, &po, "action_create_vendor_bill").await;
            bill_books::moves(&tx, company)
                .browse([bill])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell("invoice_date", Cell::Date(bill_books::d(2024, 6, 3)))
                        .set_cell("date", Cell::Date(bill_books::d(2024, 6, 3))),
                )
                .await
                .unwrap();
            made.push((po.ids()[0], bill));
        }
        let [drafted, posted, redrafted] = <[(i64, i64); 3]>::try_from(made).unwrap();
        // the redrafted order's bill holds more than was ordered
        let po = orders(&tx, &[company]).browse([redrafted.0]).unwrap();
        let line = lines_of(&mut tx, &[company], &po).await.remove(0);
        let item = item_of(&mut tx, company, redrafted.1, &line).await;
        set_quantity(&mut tx, &item, "5").await;
        tx.commit().await.unwrap();
        drop(tx);
        bill_books::periods(&orm, company).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        bill_books::moves(&tx, company)
            .browse([posted.1])
            .unwrap()
            .call::<(), ()>(&mut tx, "action_post", ())
            .await
            .unwrap();
        let number: String = tx
            .query("SELECT name FROM account_move WHERE id = $1", &[&posted.1])
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(buyer).await.unwrap();
        let quantity =
            |value: &str| RawVals::new().set_cell("product_qty", Cell::Decimal(dec(value)));
        // a posted bill: refused, naming the order but not the bill; Cancel is hidden, also on an
        // onchange snapshot of the order
        let po = orders(&tx, &[company]).browse([posted.0]).unwrap();
        let reference = name(&mut tx, &po).await;
        let kept = act(&mut tx, &po, "button_cancel").await.unwrap_err();
        assert_eq!(kept.kind(), ErrorKind::UserError, "{kept:?}");
        assert!(
            kept.message().contains(&format!(
                "The purchase order {reference} cannot be cancelled: its vendor bill is posted"
            )),
            "{kept}"
        );
        assert!(!kept.message().contains(&number), "{kept}");
        assert_eq!(cell(&mut tx, &po, "cancel_blocked").await, Cell::Bool(true));
        let model = orders(&tx, &[company]);
        let snapshot = tx
            .new_record(&model, RawVals::new(), Some(po.rec_ids()[0]), None)
            .await
            .unwrap();
        assert_eq!(
            cell(&mut tx, &snapshot, "cancel_blocked").await,
            Cell::Bool(true),
            "the snapshot of a blocked order"
        );
        // the billed floor: below it is refused with the quantities, the floor and above pass
        let line = lines_of(&mut tx, &[company], &po).await.remove(0);
        refused(
            write(&mut tx, &line, quantity("1")).await,
            ErrorKind::UserError,
            &[
                "cannot go below what was received or billed",
                "2 billed",
                "correct the bill",
            ],
        );
        write(&mut tx, &line, quantity("2")).await.unwrap();
        write(&mut tx, &line, quantity("3")).await.unwrap();
        // billed above the order: a raise passes below the floor, a lower quantity does not
        let po = orders(&tx, &[company]).browse([redrafted.0]).unwrap();
        let line = lines_of(&mut tx, &[company], &po).await.remove(0);
        write(&mut tx, &line, quantity("3")).await.unwrap();
        refused(
            write(&mut tx, &line, quantity("1")).await,
            ErrorKind::UserError,
            &["5 billed"],
        );
        // a draft bill is cancelled with its order; back to a request nothing is left to exceed
        let po = orders(&tx, &[company]).browse([drafted.0]).unwrap();
        act(&mut tx, &po, "button_cancel").await.unwrap();
        assert_eq!(state(&mut tx, &po).await, "cancel");
        assert_eq!(bill_state(&mut tx, drafted.1).await, "cancel");
        act(&mut tx, &po, "button_draft").await.unwrap();
        let line = lines_of(&mut tx, &[company], &po).await.remove(0);
        write(&mut tx, &line, quantity("1")).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        // posting is refused for a bill of a cancelled or redrafted order
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute(
            "UPDATE purchase_order SET state = 'draft' WHERE id = $1",
            &[&redrafted.0],
        )
        .await
        .unwrap();
        for bill in [drafted.1, redrafted.1] {
            let bills = bill_books::moves(&tx, company).browse([bill]).unwrap();
            refused(
                act(&mut tx, &bills, "action_post").await,
                ErrorKind::UserError,
                &["cancelled or back to a request for quotation, so it cannot be posted"],
            );
        }
        assert_eq!(bill_state(&mut tx, redrafted.1).await, "draft");
        // confirmed again, the order gets a new first bill, not the one its cancel cancelled
        let po = orders(&tx, &[company]).browse([drafted.0]).unwrap();
        act(&mut tx, &po, "button_confirm").await.unwrap();
        let again = bill(&mut tx, &po, "action_create_vendor_bill").await;
        assert_ne!(again, drafted.1);
        assert_eq!(bill_state(&mut tx, again).await, "draft");
        assert_eq!(bill_state(&mut tx, drafted.1).await, "cancel");
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    })
}

#[test]
fn service_po_bills_ordered_qty_second_receipt_creates_second_bill_and_reversal_links_po() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company = xmlid(&mut tx, "base", "main_company").await;
        let system = xmlid(&mut tx, "base", "group_system").await;
        let _admin = user(&mut tx, "cycle-billing-admin", &[system], &[company]).await;
        let purchase = xmlid(&mut tx, "purchase", "group_purchase_user").await;
        let accounting = xmlid(&mut tx, "account", "group_account_user").await;
        let clerk = user(
            &mut tx,
            "cycle-billing-clerk",
            &[purchase, accounting],
            &[company],
        )
        .await;
        bill_books::books(&mut tx, company).await;
        let k = catalog(&mut tx, company).await;

        // 1. Service product with purchase_method='receive' still bills on ordered quantities.
        let service_prod = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Installation Service"))
                        .set_cell("type", text("service"))
                        .set_cell("purchase_method", text("receive"))
                        .set_cell("uom_id", Cell::Id(k.unit))
                        .set_cell("standard_price", Cell::Decimal(dec("120"))),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let ordered_prod = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Ordered Widget"))
                        .set_cell("purchase_method", text("purchase"))
                        .set_cell("uom_id", Cell::Id(k.unit))
                        .set_cell("standard_price", Cell::Decimal(dec("15"))),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(clerk).await.unwrap();
        let svc_po = order(
            &mut tx,
            &[company],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [Command::Create(
                        line_vals(service_prod)
                            .set_cell("product_qty", Cell::Decimal(dec("2")))
                            .set_cell("price_unit", Cell::Decimal(dec("120"))),
                    )],
                ),
        )
        .await;
        act(&mut tx, &svc_po, "button_confirm").await.unwrap();
        assert_eq!(
            cell(&mut tx, &svc_po, "invoice_status").await,
            text("to invoice")
        );
        let svc_bill = bill(&mut tx, &svc_po, "action_create_vendor_bill").await;
        let svc_move = bill_books::moves(&tx, company).browse([svc_bill]).unwrap();
        assert_eq!(
            cell(&mut tx, &svc_move, "amount_untaxed").await,
            Cell::Decimal(dec("240"))
        );

        // 2. Partial bill 1 (qty 4 of 10) -> Bill 2 creates a distinct bill for remaining qty 6.
        let po = order(
            &mut tx,
            &[company],
            RawVals::new()
                .set_cell("partner_id", Cell::Id(k.vendor))
                .set_commands(
                    "order_line",
                    [Command::Create(
                        line_vals(ordered_prod)
                            .set_cell("product_qty", Cell::Decimal(dec("10")))
                            .set_cell("price_unit", Cell::Decimal(dec("15"))),
                    )],
                ),
        )
        .await;
        act(&mut tx, &po, "button_confirm").await.unwrap();
        let line = lines_of(&mut tx, &[company], &po).await.remove(0);
        let bill1 = bill(&mut tx, &po, "action_create_vendor_bill").await;
        let item1 = item_of(&mut tx, company, bill1, &line).await;
        set_quantity(&mut tx, &item1, "4").await;
        let m1 = bill_books::moves(&tx, company).browse([bill1]).unwrap();
        m1.write(
            &mut tx,
            RawVals::new().set_cell(
                "invoice_date",
                Cell::Date(Date::from_ymd(2026, 6, 1).unwrap()),
            ),
        )
        .await
        .unwrap();
        act(&mut tx, &m1, "action_post").await.unwrap();
        assert_eq!(
            cell(&mut tx, &line, "qty_invoiced").await,
            Cell::Decimal(dec("4"))
        );

        // Remaining 6 units -> action_create_vendor_bill must create a distinct 2nd bill.
        let bill2 = bill(&mut tx, &po, "action_create_vendor_bill").await;
        assert_ne!(
            bill2, bill1,
            "Remaining received qty must create a new second vendor bill"
        );
        let m2 = bill_books::moves(&tx, company).browse([bill2]).unwrap();
        assert_eq!(
            cell(&mut tx, &m2, "amount_untaxed").await,
            Cell::Decimal(dec("90"))
        );
        m2.write(
            &mut tx,
            RawVals::new().set_cell(
                "invoice_date",
                Cell::Date(Date::from_ymd(2026, 6, 2).unwrap()),
            ),
        )
        .await
        .unwrap();
        act(&mut tx, &m2, "action_post").await.unwrap();
        assert_eq!(
            cell(&mut tx, &line, "qty_invoiced").await,
            Cell::Decimal(dec("10"))
        );

        // 3. Reversing Bill 1 propagates purchase.direct.bill.line lineage to the credit note
        //    and reduces qty_invoiced on the PO line from 10 to 6 once posted.
        let rev_wiz = tx
            .env()
            .with_company(company)
            .unwrap()
            .records("account.move.reversal")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("move_id", Cell::Id(bill1))
                        .set_cell("refund_method", text("refund"))
                        .set_cell("date", Cell::Date(Date::from_ymd(2026, 6, 3).unwrap()))
                        .set_cell("reason", text("Damaged partial lot")),
                ],
            )
            .await
            .unwrap();
        let rev_action: Value = rev_wiz.call(&mut tx, "reverse_moves", ()).await.unwrap();
        let cn_id = match &rev_action {
            Value::Json(v) => v.get("res_id").and_then(|r| r.as_i64()).unwrap(),
            other => panic!("expected Json action with res_id, got {other:?}"),
        };
        let cn = bill_books::moves(&tx, company).browse([cn_id]).unwrap();
        assert_eq!(cell(&mut tx, &cn, "move_type").await, text("in_refund"));
        act(&mut tx, &cn, "action_post").await.unwrap();
        assert_eq!(
            cell(&mut tx, &line, "qty_invoiced").await,
            Cell::Decimal(dec("6")),
            "Posted vendor credit note reversal must reduce PO line qty_invoiced"
        );

        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    })
}
