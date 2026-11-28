// SPDX-License-Identifier: MIT OR Apache-2.0

use erp_fields::Decimal;

use super::*;

async fn product(tx: &mut Transaction, name: &str, kind: &str, policy: &str) -> i64 {
    tx.env()
        .records("product.product")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("type", Cell::Str(kind.into()))
                    .set_cell("invoice_policy", Cell::Str(policy.into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn line(tx: &mut Transaction, order: &Records, vals: RawVals) -> erp_fields::Result<Records> {
    let order = order.ids()[0];
    let lines = tx.env().records("sale.order.line")?;
    tx.savepoint(async move |tx| {
        let created = lines
            .create(tx, vec![vals.set_cell("order_id", Cell::Id(order))])
            .await?;
        tx.flush().await?;
        Ok(created)
    })
    .await
}

async fn product_line(tx: &mut Transaction, order: &Records, product: i64, qty: i32) -> Records {
    line(
        tx,
        order,
        RawVals::new()
            .set_cell("product_id", Cell::Id(product))
            .set_cell("product_uom_qty", Cell::Decimal(Decimal::from(qty))),
    )
    .await
    .unwrap()
}

async fn number(tx: &mut Transaction, records: &Records, field: &str) -> Decimal {
    match records.cell(tx, field).await.unwrap() {
        Cell::Decimal(value) => value,
        Cell::Int(value) => Decimal::from(value),
        other => panic!("{field} is {other:?}"),
    }
}

async fn maybe_text(tx: &mut Transaction, records: &Records, field: &str) -> Option<String> {
    match records.cell(tx, field).await.unwrap() {
        Cell::Str(value) => Some(value.to_string()),
        Cell::Null | Cell::Bool(false) => None,
        other => panic!("{field} is {other:?}"),
    }
}

#[test]
fn the_delivered_quantity_is_manual_in_sale() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let desk_line = product_line(&mut tx, &quotation, desk, 3).await;
        assert_eq!(
            text(&mut tx, &desk_line, "qty_delivered_method").await,
            "manual"
        );
        assert_eq!(
            number(&mut tx, &desk_line, "qty_delivered").await,
            Decimal::ZERO
        );
        write(
            &mut tx,
            &desk_line,
            RawVals::new().set_cell("qty_delivered", Cell::Decimal(Decimal::from(2))),
        )
        .await
        .unwrap();
        assert_eq!(
            number(&mut tx, &desk_line, "qty_delivered").await,
            Decimal::from(2)
        );
        // a section line has no product, unit, price or quantity
        let section = line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("display_type", Cell::Str("line_section".into()))
                .set_cell("name", Cell::Str("Furniture".into())),
        )
        .await
        .unwrap();
        assert_eq!(
            number(&mut tx, &section, "product_uom_qty").await,
            Decimal::ZERO
        );
        assert_eq!(many2one(&mut tx, &section, "product_uom_id").await, None);
        tx.rollback().await.unwrap();
    });
}

#[test]
fn the_quantity_to_invoice_follows_the_policy() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let hours = product(&mut tx, "Consulting", "service", "order").await;
        let boxes = product(&mut tx, "Box", "service", "delivery").await;
        let quotation = new_order(&mut tx, &world).await;
        let ordered = product_line(&mut tx, &quotation, hours, 10).await;
        let delivered = product_line(&mut tx, &quotation, boxes, 10).await;
        assert_eq!(
            number(&mut tx, &ordered, "qty_to_invoice").await,
            Decimal::ZERO,
            "a quotation"
        );
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        write(
            &mut tx,
            &delivered,
            RawVals::new().set_cell("qty_delivered", Cell::Decimal(Decimal::from(4))),
        )
        .await
        .unwrap();
        assert_eq!(
            number(&mut tx, &ordered, "qty_invoiced").await,
            Decimal::ZERO
        );
        assert_eq!(
            number(&mut tx, &ordered, "qty_to_invoice").await,
            number(&mut tx, &ordered, "product_uom_qty").await
        );
        assert_eq!(
            number(&mut tx, &delivered, "qty_to_invoice").await,
            number(&mut tx, &delivered, "qty_delivered").await
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn the_invoice_status_follows_the_lines_and_the_close_flag() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let hours = product(&mut tx, "Consulting", "service", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let hours_line = product_line(&mut tx, &quotation, hours, 10).await;
        assert_eq!(text(&mut tx, &quotation, "invoice_status").await, "no");
        refused(
            act(&mut tx, &quotation, "action_close_invoicing").await,
            ErrorKind::UserError,
            "Only a confirmed sales order can have its invoicing closed",
        );
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        assert_eq!(
            text(&mut tx, &hours_line, "invoice_status").await,
            "to invoice"
        );
        assert_eq!(
            text(&mut tx, &quotation, "invoice_status").await,
            "to invoice"
        );
        act(&mut tx, &quotation, "action_close_invoicing")
            .await
            .unwrap();
        assert_eq!(
            text(&mut tx, &quotation, "invoice_status").await,
            "invoiced"
        );
        act(&mut tx, &quotation, "action_reopen_order")
            .await
            .unwrap();
        assert_eq!(
            text(&mut tx, &quotation, "invoice_status").await,
            "to invoice"
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn the_delivery_status_follows_the_goods_lines() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let hours = product(&mut tx, "Consulting", "service", "order").await;
        let services = new_order(&mut tx, &world).await;
        product_line(&mut tx, &services, hours, 1).await;
        act(&mut tx, &services, "action_confirm").await.unwrap();
        assert_eq!(
            maybe_text(&mut tx, &services, "delivery_status").await,
            None
        );
        let quotation = new_order(&mut tx, &world).await;
        let desk_line = product_line(&mut tx, &quotation, desk, 5).await;
        write(
            &mut tx,
            &desk_line,
            RawVals::new().set_cell("qty_delivered", Cell::Decimal(Decimal::from(2))),
        )
        .await
        .unwrap();
        assert_eq!(
            maybe_text(&mut tx, &quotation, "delivery_status").await,
            None,
            "a quotation"
        );
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        assert_eq!(
            maybe_text(&mut tx, &quotation, "delivery_status")
                .await
                .as_deref(),
            Some("partial")
        );
        write(
            &mut tx,
            &desk_line,
            RawVals::new().set_cell("qty_delivered", Cell::Decimal(Decimal::from(5))),
        )
        .await
        .unwrap();
        assert_eq!(
            maybe_text(&mut tx, &quotation, "delivery_status")
                .await
                .as_deref(),
            Some("full")
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn a_line_offers_the_units_of_its_product() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        // the shipped Dozens unit is archived; an archived unit is never offered
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let dozen = tx
            .env()
            .records("uom.uom")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Box of 12".into()))
                        .set_cell("relative_uom_id", Cell::Id(unit))
                        .set_cell("relative_factor", Cell::Decimal(Decimal::from(12))),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        // the product is made with its second unit, as on the product form
        let templates = tx
            .env()
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Pens".into()))
                        .set_cell("type", Cell::Str("consu".into()))
                        .set_cell("invoice_policy", Cell::Str("order".into()))
                        .set_commands("uom_ids", [Command::set([id(dozen)])]),
                ],
            )
            .await
            .unwrap();
        let pens = templates
            .related(&mut tx, "product_variant_ids")
            .await
            .unwrap()
            .ids()[0];
        let quotation = new_order(&mut tx, &world).await;
        let pens_line = product_line(&mut tx, &quotation, pens, 1).await;
        let main = many2one(&mut tx, &pens_line, "product_uom_id")
            .await
            .unwrap();
        let allowed = |cell: Cell| match cell {
            Cell::Ids(ids) => ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
            other => panic!("allowed_uom_ids is {other:?}"),
        };
        let single = allowed(pens_line.cell(&mut tx, "allowed_uom_ids").await.unwrap());
        assert_eq!(single, [main], "one unit while several units are off");
        let feature = xmlid(&mut tx, "uom", "group_uom").await;
        admin(&mut tx).await;
        tx.env()
            .records("res.users")
            .unwrap()
            .browse(vec![SUPERUSER_ID.get()])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands("group_ids", [Command::link(id(feature))]),
            )
            .await
            .unwrap();
        let ids = pens_line.ids();
        tx.commit().await.unwrap();
        // a new transaction: the allowed units are not stored and the feature is global
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let fresh = tx
            .env()
            .records("sale.order.line")
            .unwrap()
            .browse(ids)
            .unwrap();
        let several = allowed(fresh.cell(&mut tx, "allowed_uom_ids").await.unwrap());
        assert!(
            several.contains(&main) && several.contains(&dozen),
            "{several:?}"
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn a_locked_order_keeps_its_line_prices() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let desk_line = product_line(&mut tx, &quotation, desk, 1).await;
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        act(&mut tx, &quotation, "action_lock").await.unwrap();
        let before = number(&mut tx, &desk_line, "price_unit").await;
        refused(
            write(
                &mut tx,
                &desk_line,
                RawVals::new().set_cell("price_unit", Cell::Decimal(Decimal::from(99))),
            )
            .await,
            ErrorKind::UserError,
            "These fields of its lines cannot be changed: Unit Price.",
        );
        assert_eq!(number(&mut tx, &desk_line, "price_unit").await, before);
        // the lead time is not protected
        write(
            &mut tx,
            &desk_line,
            RawVals::new().set_cell("customer_lead", Cell::Int(3)),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    });
}

/// The structural guards of a line (B3): the kind of a line does not change, the product
/// of a confirmed order's delivered line does not change, a confirmed order keeps its product
/// lines and lets its sections go.
#[test]
fn line_guards_on_kind_product_and_delete() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let chair = product(&mut tx, "Chair", "consu", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let desk_line = product_line(&mut tx, &quotation, desk, 1).await;
        let note = line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("display_type", Cell::Str("line_note".into()))
                .set_cell("name", Cell::Str("Fragile".into())),
        )
        .await
        .unwrap();
        refused(
            write(
                &mut tx,
                &note,
                RawVals::new().set_cell("display_type", Cell::Str("line_section".into())),
            )
            .await,
            ErrorKind::UserError,
            "The kind of a sales order line cannot change",
        );
        let subsection = line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("display_type", Cell::Str("line_subsection".into()))
                .set_cell("name", Cell::Str("Desks".into())),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &subsection,
            RawVals::new().set_cell("display_type", Cell::Str("line_section".into())),
        )
        .await
        .unwrap();
        // a product line needs a unit
        let unitless = line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("name", Cell::Str("Loose".into()))
                .set_cell("product_uom_id", Cell::Null),
        )
        .await;
        assert!(unitless.is_err(), "{unitless:?}");
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        write(
            &mut tx,
            &desk_line,
            RawVals::new().set_cell("qty_delivered", Cell::Decimal(Decimal::from(1))),
        )
        .await
        .unwrap();
        refused(
            write(
                &mut tx,
                &desk_line,
                RawVals::new().set_cell("product_id", Cell::Id(chair)),
            )
            .await,
            ErrorKind::UserError,
            "The product of these sales order lines can no longer be changed",
        );
        let result = tx.savepoint(async |tx| desk_line.unlink(tx).await).await;
        refused(
            result,
            ErrorKind::UserError,
            "set its quantity to 0 instead",
        );
        tx.savepoint(async |tx| note.unlink(tx).await)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    });
}

/// A product created with its name only, an order with customer and company only and a product
/// line with product and quantity only are saved: the policy, the addresses, the description and
/// the unit are required (or needed by a check) at the INSERT and are set before it.
#[test]
fn the_least_values_create_a_product_an_order_and_a_line() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let lamp = tx
            .env()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Lamp".into()))],
            )
            .await
            .unwrap();
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse(vec![world.company])
            .unwrap();
        assert_eq!(
            text(&mut tx, &lamp, "invoice_policy").await,
            text(&mut tx, &company, "sale_invoice_policy").await
        );
        let quotation = new_order(&mut tx, &world).await;
        for field in ["partner_invoice_id", "partner_shipping_id"] {
            assert_eq!(
                many2one(&mut tx, &quotation, field).await,
                Some(world.customer),
                "{field}"
            );
        }
        let lamp_line = product_line(&mut tx, &quotation, lamp.ids()[0], 2).await;
        assert_eq!(
            text(&mut tx, &lamp_line, "name").await,
            text(&mut tx, &lamp, "display_name").await
        );
        assert_eq!(
            many2one(&mut tx, &lamp_line, "product_uom_id").await,
            many2one(&mut tx, &lamp, "uom_id").await
        );
        assert!(
            many2one(&mut tx, &lamp_line, "product_uom_id")
                .await
                .is_some()
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn line_order_dates_follow_the_order_and_backfill_existing_rows() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let december = DateTime::parse_odoo("2025-12-31 23:59:59").unwrap();
        let january = DateTime::parse_odoo("2026-01-01 00:00:00").unwrap();
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("date_order", Cell::DateTime(december)),
        )
        .await
        .unwrap();
        let desk_line = product_line(&mut tx, &quotation, desk, 1).await;
        assert_eq!(
            desk_line.cell(&mut tx, "date_order").await.unwrap(),
            Cell::DateTime(december)
        );
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("date_order", Cell::DateTime(january)),
        )
        .await
        .unwrap();
        assert_eq!(
            desk_line.cell(&mut tx, "date_order").await.unwrap(),
            Cell::DateTime(january)
        );

        let line_id = desk_line.ids()[0];
        tx.execute(
            "UPDATE sale_order_line SET date_order = NULL WHERE id = $1",
            &[&line_id],
        )
        .await
        .unwrap();
        let migration = include_str!(
            "../../../../addons/sale/migrations/1.13/post-backfill-line-order-date.sql"
        );
        assert!(tx.execute(migration, &[]).await.unwrap() >= 1);
        assert_eq!(tx.execute(migration, &[]).await.unwrap(), 0);
        let matching = tx
            .query(
                "SELECT id FROM sale_order_line WHERE id = $1 AND date_order >= '2026-01-01'
               AND date_order < '2027-01-01'",
                &[&line_id],
            )
            .await
            .unwrap();
        assert_eq!(
            matching.len(),
            1,
            "the historical line is in the 2026 ranking"
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn derived_line_fields_are_refused() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "consu", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let desk_line = product_line(&mut tx, &quotation, desk, 1).await;
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        let order = quotation.ids()[0];
        let lines = tx.env().records("sale.order.line").unwrap();
        let derived: [(&str, Cell, Value); 10] = [
            (
                "date_order",
                Cell::DateTime(DateTime::now()),
                Value::DateTime(DateTime::now()),
            ),
            (
                "state",
                Cell::Str("draft".into()),
                Value::Str("draft".into()),
            ),
            (
                "company_id",
                Cell::Id(world.company),
                Value::Int(world.company),
            ),
            (
                "order_partner_id",
                Cell::Id(world.customer),
                Value::Int(world.customer),
            ),
            (
                "salesman_id",
                Cell::Id(SUPERUSER_ID.get()),
                Value::Int(SUPERUSER_ID.get()),
            ),
            ("is_downpayment", Cell::Bool(true), Value::Bool(true)),
            (
                "qty_delivered_method",
                Cell::Str("manual".into()),
                Value::Str("manual".into()),
            ),
            ("qty_invoiced", Cell::Int(1), Value::Int(1)),
            ("qty_to_invoice", Cell::Int(0), Value::Int(0)),
            (
                "invoice_status",
                Cell::Str("invoiced".into()),
                Value::Str("invoiced".into()),
            ),
        ];
        let written =
            "of a sales order line is set from its order and its operations, not written.";
        for (field, cell, value) in derived {
            for target in [desk_line.clone(), desk_line.sudo(true)] {
                refused(
                    write(
                        &mut tx,
                        &target,
                        RawVals::new().set_cell(field, cell.clone()),
                    )
                    .await,
                    ErrorKind::Validation,
                    written,
                );
            }
            for model in [lines.clone(), lines.sudo(true)] {
                let vals = RawVals::new()
                    .set_cell("order_id", Cell::Id(order))
                    .set_cell("product_id", Cell::Id(desk))
                    .set_cell(field, cell.clone());
                let result = tx
                    .savepoint(async |tx| model.create(tx, vec![vals.clone()]).await.map(|_| ()))
                    .await;
                refused(result, ErrorKind::Validation, written);
            }
            let update = Command::update(
                desk_line.rec_ids()[0],
                RawVals::new().set_cell(field, cell.clone()),
            );
            let add = Command::create(
                RawVals::new()
                    .set_cell("product_id", Cell::Id(desk))
                    .set_cell(field, cell.clone()),
            );
            for command in [update, add] {
                refused(
                    write(
                        &mut tx,
                        &quotation,
                        RawVals::new().set_commands("order_line", [command]),
                    )
                    .await,
                    ErrorKind::Validation,
                    written,
                );
            }
            let defaulted = lines.with_context([(format!("default_{field}"), value)]);
            let vals = RawVals::new()
                .set_cell("order_id", Cell::Id(order))
                .set_cell("product_id", Cell::Id(desk));
            let result = tx
                .savepoint(async |tx| defaulted.create(tx, vec![vals.clone()]).await.map(|_| ()))
                .await;
            refused(
                result,
                ErrorKind::Validation,
                &format!("The context key default_{field} is not accepted"),
            );
        }
        // the confirmed order's product line is still kept
        let result = tx.savepoint(async |tx| desk_line.unlink(tx).await).await;
        refused(
            result,
            ErrorKind::UserError,
            "set its quantity to 0 instead",
        );
        // a copy carries the lines without the derived fields
        let copied = quotation.copy(&mut tx, None).await.unwrap();
        let copied_lines = copied.related(&mut tx, "order_line").await.unwrap();
        assert_eq!(copied_lines.ids().len(), 1);
        assert_eq!(text(&mut tx, &copied_lines, "state").await, "draft");
        tx.rollback().await.unwrap();
    });
}

#[test]
fn lines_of_cancelled_and_locked_orders_do_not_change() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let hours = product(&mut tx, "Consulting", "service", "order").await;
        let quotation = new_order(&mut tx, &world).await;
        let name = text(&mut tx, &quotation, "name").await;
        let hours_line = product_line(&mut tx, &quotation, hours, 2).await;
        let note = line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("display_type", Cell::Str("line_note".into()))
                .set_cell("name", Cell::Str("Remote".into())),
        )
        .await
        .unwrap();
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        act(&mut tx, &quotation, "action_lock").await.unwrap();
        let hours_vals = || {
            RawVals::new()
                .set_cell("product_id", Cell::Id(hours))
                .set_cell("product_uom_qty", Cell::Decimal(Decimal::from(1)))
        };
        refused(
            line(&mut tx, &quotation, hours_vals()).await.map(|_| ()),
            ErrorKind::Validation,
            &format!("The sales order {name} is locked; its lines cannot be added."),
        );
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_commands("order_line", [Command::create(hours_vals())]),
            )
            .await,
            ErrorKind::Validation,
            "These fields cannot be changed: Order Lines",
        );
        let result = tx.savepoint(async |tx| note.unlink(tx).await).await;
        refused(
            result,
            ErrorKind::Validation,
            &format!("The sales order {name} is locked; its lines cannot be deleted."),
        );
        act(&mut tx, &quotation, "action_unlock").await.unwrap();
        act(&mut tx, &quotation, "action_cancel").await.unwrap();
        let cancelled = format!("The sales order {name} is cancelled; its lines cannot be");
        for (target, vals) in [
            (
                &hours_line,
                RawVals::new().set_cell("price_unit", Cell::Decimal(Decimal::from(7))),
            ),
            (
                &hours_line,
                RawVals::new().set_cell("product_uom_qty", Cell::Decimal(Decimal::from(9))),
            ),
            (
                &note,
                RawVals::new().set_cell("name", Cell::Str("Onsite".into())),
            ),
        ] {
            refused(
                write(&mut tx, target, vals).await,
                ErrorKind::Validation,
                &format!("{cancelled} changed."),
            );
        }
        refused(
            line(&mut tx, &quotation, hours_vals()).await.map(|_| ()),
            ErrorKind::Validation,
            &format!("{cancelled} added."),
        );
        for target in [&hours_line, &note] {
            let result = tx.savepoint(async |tx| target.unlink(tx).await).await;
            refused(
                result,
                ErrorKind::Validation,
                &format!("{cancelled} deleted."),
            );
        }
        // back to a quotation, the lines change again
        act(&mut tx, &quotation, "action_draft").await.unwrap();
        write(
            &mut tx,
            &hours_line,
            RawVals::new().set_cell("price_unit", Cell::Decimal(Decimal::from(7))),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    });
}

#[test]
fn customer_invoice_and_credit_line_quantities_follow_the_document() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        admin(&mut tx).await;
        let clerk = user(&mut tx, "billing-source-clerk", "group_sale_salesman").await;
        let accounting_group = xmlid(&mut tx, "account", "group_account_user").await;
        tx.env()
            .records("res.users")
            .unwrap()
            .browse([clerk])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands("group_ids", [Command::set([id(accounting_group)])]),
            )
            .await
            .unwrap();
        let sku = product(&mut tx, "Retail lamp", "consu", "order").await;
        let order = new_order(&mut tx, &world).await;
        let sale_line = product_line(&mut tx, &order, sku, 6).await;
        let other_sku = product(&mut tx, "Another lamp", "consu", "order").await;
        let other_line = product_line(&mut tx, &order, other_sku, 2).await;
        act(&mut tx, &order, "action_confirm").await.unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let (income, journal) = invoice_chart(&mut tx, &env, world.company).await;
        let sale_id = sale_line.ids()[0];
        let other_id = other_line.ids()[0];
        let journal_id = journal.ids()[0];
        tx.commit().await.unwrap();
        let secured = policy_orm(&db, &orm).await;
        let mut tx = secured.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let sale_line = env
            .records("sale.order.line")
            .unwrap()
            .browse([sale_id])
            .unwrap();
        let other_line = env
            .records("sale.order.line")
            .unwrap()
            .browse([other_id])
            .unwrap();
        let journal = env
            .records("account.journal")
            .unwrap()
            .browse([journal_id])
            .unwrap();
        let unit = many2one(&mut tx, &sale_line, "product_uom_id")
            .await
            .unwrap();
        let model = env.records("account.move").unwrap();
        let mut documents = Vec::new();
        for (kind, quantity, total) in [("out_invoice", 4, 4), ("out_refund", 1, 3)] {
            let line = RawVals::new()
                .set_cell("product_id", Cell::Id(sku))
                .set_cell("name", Cell::Str("Retail lamp".into()))
                .set_cell("product_uom_id", Cell::Id(unit))
                .set_cell("account_id", Cell::Id(income))
                .set_cell("sale_line_id", Cell::Id(sale_line.ids()[0]))
                .set_cell("quantity", Cell::Decimal(Decimal::from(quantity)))
                .set_cell("price_unit", Cell::Decimal(Decimal::from(10)));
            let doc = model
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("move_type", Cell::Str(kind.into()))
                            .set_cell("journal_id", Cell::Id(journal.ids()[0]))
                            .set_cell("partner_id", Cell::Id(world.customer))
                            .set_commands("invoice_line_ids", [Command::create(line)]),
                    ],
                )
                .await
                .unwrap();
            tx.flush().await.unwrap();
            let links = tx.query(
                "SELECT l.id::int8, l.quantity::text, l.display_type, m.state, link.sale_line_id::int8
                 FROM account_move_line l JOIN account_move m ON m.id=l.move_id
                 LEFT JOIN sale_invoice_item link ON link.invoice_item_id=l.id
                 WHERE m.id=$1 AND l.product_id=$2", &[&doc.ids()[0], &sku]
            ).await.unwrap();
            assert_eq!(links.len(), 1, "customer document retains its product line");
            let actual_source: Option<i64> = links[0].get(4);
            assert_eq!(
                actual_source,
                Some(sale_line.ids()[0]),
                "persisted invoice item source, qty {:?}, kind {:?}, state {:?}",
                links[0].get::<_, Option<String>>(1),
                links[0].get::<_, Option<String>>(2),
                links[0].get::<_, Option<String>>(3)
            );
            assert_eq!(
                number(&mut tx, &sale_line, "qty_invoiced").await,
                Decimal::from(total)
            );
            assert_eq!(
                number(&mut tx, &sale_line, "qty_to_invoice").await,
                Decimal::from(6 - total)
            );
            let item = env
                .records("account.move.line")
                .unwrap()
                .browse([links[0].get::<_, i64>(0)])
                .unwrap();
            assert_eq!(
                many2one(&mut tx, &item, "sale_line_id").await,
                Some(sale_line.ids()[0])
            );
            let clerk_env = env.with_user(clerk).with_company(world.company).unwrap();
            let clerk_item = item.with_env(clerk_env.clone());
            clerk_item
                .check_access(&mut tx, erp_sdk::Operation::Write)
                .await
                .unwrap();
            let private = clerk_env.records("sale.invoice.item").unwrap();
            let private_denied = tx
                .savepoint(async |tx| {
                    private
                        .create(
                            tx,
                            vec![
                                RawVals::new()
                                    .set_cell("sale_line_id", Cell::Id(sale_line.ids()[0]))
                                    .set_cell("invoice_item_id", Cell::Id(item.ids()[0])),
                            ],
                        )
                        .await
                        .map(|_| ())
                })
                .await
                .expect_err("accounting cannot directly create sales associations");
            assert_eq!(private_denied.kind(), ErrorKind::Access);
            let source_denied = write(
                &mut tx,
                &clerk_item,
                RawVals::new().set_cell("sale_line_id", Cell::Null),
            )
            .await
            .expect_err("accounting without Sales cannot detach the source");
            assert_eq!(source_denied.kind(), ErrorKind::Access);
            refused(
                write(
                    &mut tx,
                    &item,
                    RawVals::new().set_cell("sale_line_id", Cell::Id(other_line.ids()[0])),
                )
                .await,
                ErrorKind::Validation,
                "must match",
            );
            assert_eq!(
                many2one(&mut tx, &item, "sale_line_id").await,
                Some(sale_line.ids()[0])
            );
            assert_eq!(
                number(&mut tx, &other_line, "qty_invoiced").await,
                Decimal::ZERO
            );
            documents.push(doc);
        }
        act(&mut tx, &documents[0], "button_cancel").await.unwrap();
        tx.flush().await.unwrap();
        assert_eq!(
            number(&mut tx, &sale_line, "qty_invoiced").await,
            Decimal::NEGATIVE_ONE
        );
        act(&mut tx, &documents[0], "button_draft").await.unwrap();
        tx.flush().await.unwrap();
        assert_eq!(
            number(&mut tx, &sale_line, "qty_invoiced").await,
            Decimal::from(3)
        );
        documents[1].unlink(&mut tx).await.unwrap();
        tx.flush().await.unwrap();
        assert_eq!(
            number(&mut tx, &sale_line, "qty_invoiced").await,
            Decimal::from(4)
        );
        act(&mut tx, &documents[0], "action_post").await.unwrap();
        tx.flush().await.unwrap();
        assert_eq!(
            number(&mut tx, &sale_line, "qty_invoiced").await,
            Decimal::from(4)
        );
        tx.rollback().await.unwrap();
    });
}

pub(super) async fn invoice_chart(
    tx: &mut Transaction,
    env: &erp_sdk::Env,
    company: i64,
) -> (i64, Records) {
    let accounts = env.records("account.account").unwrap();
    let mut income = 0;
    for (code, kind) in [("SIB400", "income"), ("SIB121", "asset_receivable")] {
        let account = accounts
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("name", Cell::Str(code.into()))
                        .set_cell("account_type", Cell::Str(kind.into())),
                ],
            )
            .await
            .unwrap();
        if kind == "income" {
            income = account.ids()[0];
        }
    }
    let journal = env
        .records("account.journal")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Retail invoices".into()))
                    .set_cell("code", Cell::Str("SIB".into()))
                    .set_cell("type", Cell::Str("sale".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("default_account_id", Cell::Id(income)),
            ],
        )
        .await
        .unwrap();
    (income, journal)
}

#[test]
fn confirmed_order_creates_a_single_draft_customer_invoice_at_its_terms() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let env = tx.env().with_company(world.company).unwrap();
        let (_income, _journal) = invoice_chart(&mut tx, &env, world.company).await;
        admin(&mut tx).await;
        let clerk = user(
            &mut tx,
            "ordinary-billing-clerk",
            "group_sale_salesman_all_leads",
        )
        .await;
        let seller = user(
            &mut tx,
            "sales-only-invoice-clerk",
            "group_sale_salesman_all_leads",
        )
        .await;
        let accounting = xmlid(&mut tx, "account", "group_account_user").await;
        env.records("res.users")
            .unwrap()
            .browse([clerk])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands("group_ids", [Command::link(id(accounting))]),
            )
            .await
            .unwrap();
        super::prices::company_country(&mut tx, world.company).await;
        let tax = super::prices::tax(&mut tx, world.company, "Invoice sales tax", "10").await;
        let sku = product(&mut tx, "Reading lamp", "consu", "order").await;
        let (_, foreign, pricelist) = super::prices::foreign_pricelist(&mut tx, &world).await;
        let order = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await
        .with_env(env.clone());
        let sale_line = line(
            &mut tx,
            &order,
            RawVals::new()
                .set_cell("product_id", Cell::Id(sku))
                .set_cell("price_unit", Cell::Decimal(Decimal::from(20)))
                .set_cell("product_uom_qty", Cell::Decimal(Decimal::from(3))),
        )
        .await
        .unwrap();
        assert_eq!(
            many2one(&mut tx, &order, "currency_id").await,
            Some(foreign)
        );
        write(
            &mut tx,
            &sale_line,
            RawVals::new()
                .set_cell("product_uom_qty", Cell::Decimal(Decimal::new(301, 2)))
                .set_cell("price_unit", Cell::Decimal(Decimal::from(20)))
                .set_cell("discount", Cell::Decimal(Decimal::from(10))),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &sale_line,
            RawVals::new().set_commands("tax_ids", [Command::set([id(tax)])]),
        )
        .await
        .unwrap();
        act(&mut tx, &order, "action_confirm").await.unwrap();
        let order_id = order.ids()[0];
        let line_id = sale_line.ids()[0];
        tx.commit().await.unwrap();
        let secured = policy_orm(&db, &orm).await;
        let mut tx = secured.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let order = env
            .records("sale.order")
            .unwrap()
            .browse([order_id])
            .unwrap();
        let sale_line = env
            .records("sale.order.line")
            .unwrap()
            .browse([line_id])
            .unwrap();
        let seller_order =
            order.with_env(env.with_user(seller).with_company(world.company).unwrap());
        let denied = tx
            .savepoint(async |tx| {
                let result: Value = seller_order
                    .call(tx, "action_create_draft_invoice", ())
                    .await?;
                Ok(result)
            })
            .await
            .expect_err("sales-only cannot create accounting documents");
        assert_eq!(denied.kind(), ErrorKind::Access, "{denied}");
        let order = order.with_env(env.with_user(clerk).with_company(world.company).unwrap());
        assert_eq!(
            order.cell(&mut tx, "invoice_count").await.unwrap(),
            Cell::Int(0)
        );
        let action: Value = order
            .call(&mut tx, "action_create_draft_invoice", ())
            .await
            .unwrap();
        let normalized: Value = order
            .call(&mut tx, "erp_normalize_action_result", (1_u64, action))
            .await
            .expect("the same web action normalizer used by the form button accepts the result");
        let Value::Json(envelope) = normalized else {
            panic!("a web action envelope opens the draft")
        };
        let action = &envelope["result"];
        assert_eq!(action["kind"], "window");
        assert_eq!(action["model"], "account.move");
        assert_eq!(action["views"][0]["type"], "form");
        assert_eq!(
            action["views"][0]["id"],
            xmlid(&mut tx, "account", "view_invoice_form").await
        );
        let invoice = action["res_id"].as_i64().expect("a persisted invoice");
        let doc = env
            .records("account.move")
            .unwrap()
            .browse([invoice])
            .unwrap();
        assert_eq!(
            doc.cell(&mut tx, "state").await.unwrap(),
            Cell::Str("draft".into())
        );
        assert_eq!(
            many2one(&mut tx, &doc, "partner_id").await,
            Some(world.customer)
        );
        assert_eq!(
            many2one(&mut tx, &doc, "currency_id").await,
            many2one(&mut tx, &order, "currency_id").await
        );
        assert_eq!(
            number(&mut tx, &doc, "amount_total").await,
            Decimal::new(5960, 2)
        );
        assert_eq!(
            number(&mut tx, &sale_line, "qty_invoiced").await,
            Decimal::new(301, 2)
        );
        let retry = tx
            .savepoint(async |tx| {
                let value: Value = order.call(tx, "action_create_draft_invoice", ()).await?;
                Ok(value)
            })
            .await
            .unwrap_err();
        assert!(retry.message().contains("remaining quantities"), "{retry}");
        let count: i64 = tx
            .query(
                "SELECT COUNT(*) FROM account_move WHERE move_type='out_invoice'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(count, 1);
        // The count button and its way back: one invoice, shown to the clerk, hidden from sales.
        assert_eq!(
            order.cell(&mut tx, "invoice_count").await.unwrap(),
            Cell::Int(1)
        );
        assert_eq!(
            seller_order.cell(&mut tx, "invoice_count").await.unwrap(),
            Cell::Int(0)
        );
        assert_eq!(
            doc.cell(&mut tx, "sale_order_count").await.unwrap(),
            Cell::Int(1)
        );
        let hidden = doc.with_env(env.with_user(seller).with_company(world.company).unwrap());
        // Without read on the invoice model the invoice itself is closed to that user.
        let closed = hidden
            .cell(&mut tx, "sale_order_count")
            .await
            .expect_err("a sales-only user cannot read invoices");
        assert_eq!(closed.kind(), ErrorKind::Access, "{closed}");
        let none: Value = seller_order
            .call(&mut tx, "action_view_invoice", ())
            .await
            .unwrap();
        let Value::Map(none) = none else {
            panic!("an action dictionary")
        };
        assert!(
            !format!("{:?}", none["domain"]).contains("Int("),
            "{none:?}"
        );
        let shown: Value = order
            .call(&mut tx, "action_view_invoice", ())
            .await
            .unwrap();
        let shown: Value = order
            .call(&mut tx, "erp_normalize_action_result", (1_u64, shown))
            .await
            .expect("the web action normalizer accepts the invoices action");
        let Value::Json(shown) = shown else {
            panic!("a web action envelope")
        };
        assert_eq!(shown["result"]["kind"], "current_window");
        assert_eq!(shown["result"]["model"], "account.move");
        // One invoice: the button opens its form (the invoice form view), not a list.
        assert_eq!(shown["result"]["res_id"], invoice);
        assert_eq!(shown["result"]["views"].as_array().unwrap().len(), 1);
        assert_eq!(shown["result"]["views"][0]["type"], "form");
        assert!(shown["result"]["views"][0]["id"].is_i64());
        let back: Value = doc
            .with_env(env.with_user(clerk).with_company(world.company).unwrap())
            .call(&mut tx, "action_view_sale_orders", ())
            .await
            .unwrap();
        let back: Value = order
            .call(&mut tx, "erp_normalize_action_result", (1_u64, back))
            .await
            .expect("the web action normalizer accepts the sales orders action");
        let Value::Json(back) = back else {
            panic!("a web action envelope")
        };
        assert_eq!(back["result"]["model"], "sale.order");
        assert_eq!(back["result"]["views"][0]["type"], "form");
        assert!(back["result"]["res_id"].is_i64());
        // A second invoice for added quantity makes the count two, still one order.
        write(
            &mut tx,
            &sale_line,
            RawVals::new().set_cell("product_uom_qty", Cell::Decimal(Decimal::from(6))),
        )
        .await
        .unwrap();
        let _: Value = order
            .call(&mut tx, "action_create_draft_invoice", ())
            .await
            .unwrap();
        // The count read before follows the new invoice in the same transaction.
        assert_eq!(
            order.cell(&mut tx, "invoice_count").await.unwrap(),
            Cell::Int(2)
        );
        // Two invoices open the list again.
        let shown: Value = order
            .call(&mut tx, "action_view_invoice", ())
            .await
            .unwrap();
        let shown: Value = order
            .call(&mut tx, "erp_normalize_action_result", (1_u64, shown))
            .await
            .expect("the web action normalizer accepts the invoices list");
        let Value::Json(shown) = shown else {
            panic!("a web action envelope")
        };
        assert_eq!(shown["result"]["views"].as_array().unwrap().len(), 2);
        assert!(shown["result"].get("res_id").is_none());
        assert_eq!(shown["result"]["domain"][0][2].as_array().unwrap().len(), 2);
        // Both counts follow a removed source in the same transaction: the first invoice bills
        // no order any more, and the order keeps one invoice.
        let sourced: Vec<i64> = tx
            .query(
                "SELECT l.id::int8 FROM account_move_line l \
                 JOIN sale_invoice_item k ON k.invoice_item_id = l.id WHERE l.move_id = $1",
                &[&invoice],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(!sourced.is_empty());
        write(
            &mut tx,
            &env.records("account.move.line")
                .unwrap()
                .browse(sourced)
                .unwrap(),
            RawVals::new().set_cell("sale_line_id", Cell::Null),
        )
        .await
        .unwrap();
        assert_eq!(
            doc.cell(&mut tx, "sale_order_count").await.unwrap(),
            Cell::Int(0)
        );
        assert_eq!(
            order.cell(&mut tx, "invoice_count").await.unwrap(),
            Cell::Int(1)
        );
        tx.rollback().await.unwrap();
    });
}
