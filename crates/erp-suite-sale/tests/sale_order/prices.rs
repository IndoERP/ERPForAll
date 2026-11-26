// SPDX-License-Identifier: MIT OR Apache-2.0
//! Prices, taxes and amounts of the installed `sale` addon: the line price from the pricelist or
//! the product, the manual price, the discount, the taxes with their fiscal position, the line and
//! order amounts, the order currency and its rate, and the derived fields no client writes
//! (reference behaviour and
//! ).

use erp_fields::{Date, Decimal};

use super::*;

pub(super) fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

async fn amount(tx: &mut Transaction, records: &Records, field: &str) -> Decimal {
    match records.cell(tx, field).await.unwrap() {
        Cell::Decimal(value) => value.normalize(),
        Cell::Int(value) => Decimal::from(value),
        other => panic!("{field} is {other:?}"),
    }
}

async fn ids_of(tx: &mut Transaction, records: &Records, field: &str) -> Vec<i64> {
    match records.cell(tx, field).await.unwrap() {
        Cell::Ids(ids) => ids.iter().map(|id| id.as_real().unwrap().get()).collect(),
        other => panic!("{field} is {other:?}"),
    }
}

/// Creates in the main company, where the accounting records of these cases live.
async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    let company = xmlid(tx, "base", "main_company").await;
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

pub(super) async fn tax(tx: &mut Transaction, company: i64, name: &str, percent: &str) -> i64 {
    // a tax needs a tax group, and a plain install ships none
    let group = create(
        tx,
        "account.tax.group",
        RawVals::new().set_cell("name", Cell::Str(format!("{name} group").into())),
    )
    .await;
    // taxes are configured inside a company
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("amount_type", Cell::Str("percent".into()))
                    .set_cell("amount", Cell::Decimal(dec(percent)))
                    .set_cell("type_tax_use", Cell::Str("sale".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("tax_group_id", Cell::Id(group)),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

/// A product in Units at the sales price `price` with the sales taxes `taxes`.
pub(super) async fn product(tx: &mut Transaction, name: &str, price: &str, taxes: &[i64]) -> i64 {
    let unit = xmlid(tx, "uom", "product_uom_unit").await;
    create(
        tx,
        "product.product",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("list_price", Cell::Decimal(dec(price)))
            .set_cell("uom_id", Cell::Id(unit))
            .set_commands(
                "taxes_id",
                [Command::set(
                    taxes.iter().map(|t| id(*t)).collect::<Vec<_>>(),
                )],
            ),
    )
    .await
}

/// [`add_line`] in a savepoint, so a refusal leaves the transaction usable.
async fn try_line(
    tx: &mut Transaction,
    order: &Records,
    vals: RawVals,
) -> erp_fields::Result<Records> {
    let vals = vals.set_cell("order_id", Cell::Id(order.ids()[0]));
    tx.savepoint(async move |tx| {
        tx.env()
            .records("sale.order.line")?
            .create(tx, vec![vals])
            .await
    })
    .await
}

pub(super) async fn add_line(tx: &mut Transaction, order: &Records, vals: RawVals) -> Records {
    tx.env()
        .records("sale.order.line")
        .unwrap()
        .create(
            tx,
            vec![vals.set_cell("order_id", Cell::Id(order.ids()[0]))],
        )
        .await
        .unwrap()
}

#[test]
fn line_prices_come_from_the_pricelist_or_the_product_and_a_manual_price_stays() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let dozen = xmlid(&mut tx, "uom", "product_uom_dozen").await;
        let chair = product(&mut tx, "Chair", "100", &[]).await;
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap();
        let currency = many2one(&mut tx, &company, "currency_id").await.unwrap();
        let pricelist = create(
            &mut tx,
            "product.pricelist",
            RawVals::new()
                .set_cell("name", Cell::Str("Partners".into()))
                .set_cell("currency_id", Cell::Id(currency)),
        )
        .await;
        create(
            &mut tx,
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("compute_price", Cell::Str("fixed".into()))
                .set_cell("fixed_price", Cell::Decimal(dec("80"))),
        )
        .await;

        let priced = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await;
        let line = add_line(
            &mut tx,
            &priced,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("80"));
        assert_eq!(
            amount(&mut tx, &line, "technical_price_unit").await,
            dec("80")
        );

        // without a pricelist: the sales price, per unit of the line
        let plain = new_order(&mut tx, &world).await;
        let manual = add_line(
            &mut tx,
            &plain,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        let automatic = add_line(
            &mut tx,
            &plain,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        assert_eq!(amount(&mut tx, &automatic, "price_unit").await, dec("100"));
        write(
            &mut tx,
            &manual,
            RawVals::new().set_cell("price_unit", Cell::Decimal(dec("50"))),
        )
        .await
        .unwrap();
        for line in [&manual, &automatic] {
            write(
                &mut tx,
                line,
                RawVals::new().set_cell("product_uom_id", Cell::Id(dozen)),
            )
            .await
            .unwrap();
        }
        assert_eq!(amount(&mut tx, &manual, "price_unit").await, dec("50"));
        assert_eq!(amount(&mut tx, &automatic, "price_unit").await, dec("1200"));
    });
}

#[test]
fn taxes_line_amounts_and_order_totals() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        // taxes belong to the company's country
        let us = xmlid(&mut tx, "base", "us").await;
        let company_partner = xmlid(&mut tx, "base", "main_partner").await;
        tx.env()
            .with_company(world.company)
            .unwrap()
            .records("res.partner")
            .unwrap()
            .browse([company_partner])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("country_id", Cell::Id(us)))
            .await
            .unwrap();
        let ten = tax(&mut tx, world.company, "Sales 10%", "10").await;
        let fifteen = tax(&mut tx, world.company, "Sales 15%", "15").await;
        let export = tax(&mut tx, world.company, "Export 0%", "0").await;
        let default = tax(&mut tx, world.company, "Default 5%", "5").await;
        let desk = product(&mut tx, "Desk", "100", &[ten]).await;
        let lamp = product(&mut tx, "Lamp", "100", &[fifteen]).await;
        let untaxed = product(&mut tx, "Voucher", "30", &[]).await;

        // 023 and 024: two lines and the order totals
        let quotation = new_order(&mut tx, &world).await;
        let first = add_line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("product_id", Cell::Id(desk))
                .set_cell("product_uom_qty", Cell::Decimal(Decimal::from(2)))
                .set_cell("discount", Cell::Decimal(dec("10"))),
        )
        .await;
        assert_eq!(ids_of(&mut tx, &first, "tax_ids").await, vec![ten]);
        assert_eq!(amount(&mut tx, &first, "price_subtotal").await, dec("180"));
        assert_eq!(amount(&mut tx, &first, "price_tax").await, dec("18"));
        assert_eq!(amount(&mut tx, &first, "price_total").await, dec("198"));
        add_line(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("product_id", Cell::Id(desk)),
        )
        .await;
        assert_eq!(
            amount(&mut tx, &quotation, "amount_untaxed").await,
            dec("280")
        );
        assert_eq!(amount(&mut tx, &quotation, "amount_tax").await, dec("28"));
        assert_eq!(
            amount(&mut tx, &quotation, "amount_total").await,
            dec("308")
        );
        // a deleted line leaves the totals
        first.unlink(&mut tx).await.unwrap();
        assert_eq!(
            amount(&mut tx, &quotation, "amount_total").await,
            dec("110")
        );

        // 022: the fiscal position maps the product's 15 % to the export tax
        let position = create(
            &mut tx,
            "account.fiscal.position",
            RawVals::new()
                .set_cell("name", Cell::Str("Export".into()))
                .set_cell("company_id", Cell::Id(world.company)),
        )
        .await;
        tx.env()
            .with_company(world.company)
            .unwrap()
            .records("account.tax")
            .unwrap()
            .browse([export])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_commands("fiscal_position_ids", [Command::set([id(position)])])
                    .set_commands("original_tax_ids", [Command::set([id(fifteen)])]),
            )
            .await
            .unwrap();
        let exported = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("fiscal_position_id", Cell::Id(position)),
        )
        .await;
        let mapped = add_line(
            &mut tx,
            &exported,
            RawVals::new().set_cell("product_id", Cell::Id(lamp)),
        )
        .await;
        assert_eq!(ids_of(&mut tx, &mapped, "tax_ids").await, vec![export]);
        assert_eq!(amount(&mut tx, &mapped, "price_tax").await, dec("0"));

        // 037: the company default reaches only a productless ordinary line
        tx.env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("account_sale_tax_id", Cell::Id(default)),
            )
            .await
            .unwrap();
        // an ordinary line without a product still needs a unit
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let mixed = new_order(&mut tx, &world).await;
        let voucher = add_line(
            &mut tx,
            &mixed,
            RawVals::new().set_cell("product_id", Cell::Id(untaxed)),
        )
        .await;
        let service = add_line(
            &mut tx,
            &mixed,
            RawVals::new()
                .set_cell("name", Cell::Str("Installation".into()))
                .set_cell("product_uom_id", Cell::Id(unit))
                .set_cell("price_unit", Cell::Decimal(dec("40"))),
        )
        .await;
        let note = add_line(
            &mut tx,
            &mixed,
            RawVals::new()
                .set_cell("display_type", Cell::Str("line_note".into()))
                .set_cell("name", Cell::Str("Delivered on site".into())),
        )
        .await;
        assert!(ids_of(&mut tx, &voucher, "tax_ids").await.is_empty());
        assert_eq!(ids_of(&mut tx, &service, "tax_ids").await, vec![default]);
        assert!(ids_of(&mut tx, &note, "tax_ids").await.is_empty());
        assert_eq!(amount(&mut tx, &mixed, "amount_untaxed").await, dec("70"));
        assert_eq!(amount(&mut tx, &mixed, "amount_tax").await, dec("2"));
    });
}

pub(super) async fn foreign_pricelist(tx: &mut Transaction, world: &World) -> (i64, i64, i64) {
    let companies = tx.env().records("res.company").unwrap();
    let company = companies.browse([world.company]).unwrap();
    let home = many2one(tx, &company, "currency_id").await.unwrap();
    let eur = xmlid(tx, "base", "EUR").await;
    let usd = xmlid(tx, "base", "USD").await;
    let foreign = if home == eur { usd } else { eur };
    tx.env()
        .records("res.currency")
        .unwrap()
        .browse([foreign])
        .unwrap()
        .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    for (currency, rate) in [(home, "1"), (foreign, "0.9")] {
        create(
            tx,
            "res.currency.rate",
            RawVals::new()
                .set_cell("name", Cell::Date(Date::from_ymd(2026, 1, 1).unwrap()))
                .set_cell("rate", Cell::Decimal(dec(rate)))
                .set_cell("currency_id", Cell::Id(currency))
                .set_cell("company_id", Cell::Id(world.company)),
        )
        .await;
    }
    let pricelist = create(
        tx,
        "product.pricelist",
        RawVals::new()
            .set_cell("name", Cell::Str("Abroad".into()))
            .set_cell("currency_id", Cell::Id(foreign)),
    )
    .await;
    (home, foreign, pricelist)
}

#[test]
fn currency_follows_the_pricelist_and_a_confirmed_pricelist_is_kept() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let (home, foreign, pricelist) = foreign_pricelist(&mut tx, &world).await;
        let abroad = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await;
        assert_eq!(
            many2one(&mut tx, &abroad, "currency_id").await,
            Some(foreign)
        );
        assert_eq!(amount(&mut tx, &abroad, "currency_rate").await, dec("0.9"));
        let home_order = new_order(&mut tx, &world).await;
        assert_eq!(
            many2one(&mut tx, &home_order, "currency_id").await,
            Some(home)
        );
        assert_eq!(
            amount(&mut tx, &home_order, "currency_rate").await,
            dec("1")
        );

        // no rule of the foreign pricelist: converts list_price (100) from company currency at 0.9
        let lamp = product(&mut tx, "Lamp", "100", &[]).await;
        let converted = add_line(
            &mut tx,
            &abroad,
            RawVals::new().set_cell("product_id", Cell::Id(lamp)),
        )
        .await;
        assert_eq!(amount(&mut tx, &converted, "price_unit").await, dec("90"));
        // a typed price is the caller's and is kept
        let typed = add_line(
            &mut tx,
            &abroad,
            RawVals::new()
                .set_cell("product_id", Cell::Id(lamp))
                .set_cell("price_unit", Cell::Decimal(dec("95"))),
        )
        .await;
        assert_eq!(amount(&mut tx, &typed, "price_unit").await, dec("95"));
        // a fixed-price rule is in the pricelist currency and prices the line
        create(
            &mut tx,
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                .set_cell("product_id", Cell::Id(lamp))
                .set_cell("compute_price", Cell::Str("fixed".into()))
                .set_cell("fixed_price", Cell::Decimal(dec("90"))),
        )
        .await;
        let fixed = add_line(
            &mut tx,
            &abroad,
            RawVals::new().set_cell("product_id", Cell::Id(lamp)),
        )
        .await;
        assert_eq!(amount(&mut tx, &fixed, "price_unit").await, dec("90"));

        act(&mut tx, &abroad, "action_confirm").await.unwrap();
        refused(
            write(
                &mut tx,
                &abroad,
                RawVals::new().set_cell("pricelist_id", Cell::Null),
            )
            .await,
            ErrorKind::UserError,
            "cannot be changed after it is confirmed",
        );
        assert_eq!(
            many2one(&mut tx, &abroad, "pricelist_id").await,
            Some(pricelist)
        );
    });
}

#[test]
fn derived_amounts_are_never_written_by_a_client() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let desk = product(&mut tx, "Desk", "100", &[]).await;
        let quotation = new_order(&mut tx, &world).await;
        let line = add_line(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("product_id", Cell::Id(desk)),
        )
        .await;
        let message = "is computed from the order and its lines and cannot be written";
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("amount_total", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::Validation,
            message,
        );
        refused(
            write(
                &mut tx,
                &line,
                RawVals::new().set_cell("price_subtotal", Cell::Decimal(dec("1"))),
            )
            .await,
            ErrorKind::Validation,
            message,
        );
        // through an order command of a new line, a create with the field, and a context default
        let orders = tx.env().records("sale.order").unwrap();
        let attempts: Vec<(Records, RawVals)> = vec![
            (
                orders.clone(),
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(world.customer))
                    .set_cell("company_id", Cell::Id(world.company))
                    .set_commands(
                        "order_line",
                        [Command::Create(
                            RawVals::new()
                                .set_cell("product_id", Cell::Id(desk))
                                .set_cell("price_total", Cell::Decimal(dec("1"))),
                        )],
                    ),
            ),
            (
                orders.clone(),
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(world.customer))
                    .set_cell("company_id", Cell::Id(world.company))
                    .set_cell("currency_rate", Cell::Decimal(dec("2"))),
            ),
            (
                orders.with_context([("default_amount_untaxed", Value::Int(5))]),
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(world.customer))
                    .set_cell("company_id", Cell::Id(world.company)),
            ),
            (
                tx.env()
                    .records("sale.order.line")
                    .unwrap()
                    .with_context([("default_price_tax", Value::Int(5))]),
                RawVals::new()
                    .set_cell("order_id", Cell::Id(quotation.ids()[0]))
                    .set_cell("product_id", Cell::Id(desk)),
            ),
        ];
        for (model, vals) in attempts {
            let result = tx
                .savepoint(async move |tx| model.create(tx, vec![vals]).await.map(|_| ()))
                .await;
            refused(result, ErrorKind::Validation, message);
        }
        assert_eq!(
            amount(&mut tx, &quotation, "amount_total").await,
            dec("100")
        );
    });
}

#[test]
fn a_line_from_the_form_is_priced_and_a_given_price_wins() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let chair = product(&mut tx, "Chair", "100", &[]).await;
        let desk = product(&mut tx, "Desk", "300", &[]).await;
        let quotation = new_order(&mut tx, &world).await;
        // what the line form sends: its default price, discount and technical price, untouched or typed
        let form_line = |price: &str| {
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("product_uom_qty", Cell::Decimal(Decimal::ONE))
                .set_cell("price_unit", Cell::Decimal(dec(price)))
                .set_cell("technical_price_unit", Cell::Decimal(dec("0")))
                .set_cell("discount", Cell::Decimal(dec("0")))
        };
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_commands(
                "order_line",
                [
                    Command::Create(form_line("0")),
                    Command::Create(form_line("55")),
                ],
            ),
        )
        .await
        .unwrap();
        let lines = quotation.related(&mut tx, "order_line").await.unwrap();
        let ids = lines.ids();
        assert_eq!(ids.len(), 2);
        let (untouched, typed) = (
            lines.browse([ids[0]]).unwrap(),
            lines.browse([ids[1]]).unwrap(),
        );
        assert_eq!(amount(&mut tx, &untouched, "price_unit").await, dec("100"));
        assert_eq!(amount(&mut tx, &typed, "price_unit").await, dec("55"));
        // over RPC: no price is priced, a price given without the technical price is kept
        let plain = add_line(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        assert_eq!(amount(&mut tx, &plain, "price_unit").await, dec("100"));
        let given = add_line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("price_unit", Cell::Decimal(dec("70"))),
        )
        .await;
        assert_eq!(amount(&mut tx, &given, "price_unit").await, dec("70"));

        // a price given with a new product is the one kept
        write(
            &mut tx,
            &untouched,
            RawVals::new()
                .set_cell("product_id", Cell::Id(desk))
                .set_cell("price_unit", Cell::Decimal(dec("120"))),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &untouched, "price_unit").await, dec("120"));
        // a price typed in the form was set by hand and survives a new product
        write(
            &mut tx,
            &typed,
            RawVals::new().set_cell("product_id", Cell::Id(desk)),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &typed, "price_unit").await, dec("55"));
        // a priced line takes the new product's price
        write(
            &mut tx,
            &plain,
            RawVals::new().set_cell("product_id", Cell::Id(desk)),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &plain, "price_unit").await, dec("300"));
        assert_eq!(
            amount(&mut tx, &quotation, "amount_untaxed").await,
            dec("545")
        );
        tx.rollback().await.unwrap();
    });
}

/// Gives the company a country, which its taxes need.
pub(super) async fn company_country(tx: &mut Transaction, company: i64) {
    let us = xmlid(tx, "base", "us").await;
    let company_partner = xmlid(tx, "base", "main_partner").await;
    tx.env()
        .with_company(company)
        .unwrap()
        .records("res.partner")
        .unwrap()
        .browse([company_partner])
        .unwrap()
        .write(tx, RawVals::new().set_cell("country_id", Cell::Id(us)))
        .await
        .unwrap();
}

/// empty tax set; the product's description and taxes are still taken)
#[test]
fn a_line_made_by_the_form_takes_description_and_taxes_from_the_product() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        company_country(&mut tx, world.company).await;
        let vat = tax(&mut tx, world.company, "Form VAT", "10").await;
        let chair = product(&mut tx, "Form chair", "100", &[vat]).await;
        let quotation = new_order(&mut tx, &world).await;
        let line = add_line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("product_uom_qty", Cell::Decimal(dec("2"))),
        )
        .await;
        assert_eq!(ids_of(&mut tx, &line, "tax_ids").await, vec![vat]);
        // a description sent as null (an emptied field) is not given either
        let emptied = add_line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("name", Cell::Null),
        )
        .await;
        assert!(matches!(
            emptied.cell(&mut tx, "name").await.unwrap(),
            Cell::Str(ref name) if name.contains("Form chair")
        ));
        // a tax set that is given, the empty one too, is kept
        let taxless = add_line(
            &mut tx,
            &quotation,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_commands("tax_ids", [Command::set([])]),
        )
        .await;
        assert!(ids_of(&mut tx, &taxless, "tax_ids").await.is_empty());
        // a copy keeps the taxed line taxed and the taxless one taxless
        let copied = quotation.copy(&mut tx, None).await.unwrap();
        let copied_lines = copied.related(&mut tx, "order_line").await.unwrap();
        let mut counts = Vec::new();
        for line in copied_lines.ids() {
            let one = copied_lines.browse(vec![line]).unwrap();
            counts.push(ids_of(&mut tx, &one, "tax_ids").await.len());
        }
        counts.sort_unstable();
        assert_eq!(counts, vec![0, 1, 1]);
        assert_eq!(amount(&mut tx, &line, "price_total").await, dec("220"));
        let described = line.cell(&mut tx, "name").await.unwrap();
        assert!(matches!(described, Cell::Str(ref name) if name.contains("Form chair")));
        // a line with neither product nor description is refused by name
        refused(
            try_line(&mut tx, &quotation, RawVals::new())
                .await
                .map(|_| ()),
            ErrorKind::Validation,
            "A line needs a product or a description.",
        );
        tx.rollback().await.unwrap();
    });
}

/// unit, so one dozen crosses a minimum of twelve units)
#[test]
fn a_quantity_in_dozens_crosses_a_pricelist_minimum_in_units() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let dozen = xmlid(&mut tx, "uom", "product_uom_dozen").await;
        let chair = product(&mut tx, "Bulk chair", "100", &[]).await;
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap();
        let currency = many2one(&mut tx, &company, "currency_id").await.unwrap();
        let pricelist = create(
            &mut tx,
            "product.pricelist",
            RawVals::new()
                .set_cell("name", Cell::Str("Bulk".into()))
                .set_cell("currency_id", Cell::Id(currency)),
        )
        .await;
        create(
            &mut tx,
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("min_quantity", Cell::Decimal(dec("12")))
                .set_cell("compute_price", Cell::Str("fixed".into()))
                .set_cell("fixed_price", Cell::Decimal(dec("80"))),
        )
        .await;
        let priced = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await;
        let line = add_line(
            &mut tx,
            &priced,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("product_uom_id", Cell::Id(dozen))
                .set_cell("product_uom_qty", Cell::Decimal(dec("1"))),
        )
        .await;
        // 80 per unit at the minimum, twelve units in the dozen
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("960"));
        tx.rollback().await.unwrap();
    });
}

/// the prices and the amounts)
#[test]
fn an_order_created_with_lines_and_its_copy_keep_prices_and_amounts() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        company_country(&mut tx, world.company).await;
        let vat = tax(&mut tx, world.company, "One call VAT", "10").await;
        let chair = product(&mut tx, "One call chair", "100", &[vat]).await;
        let quotation = order(
            &mut tx,
            &world,
            RawVals::new().set_commands(
                "order_line",
                [Command::create(
                    RawVals::new()
                        .set_cell("product_id", Cell::Id(chair))
                        .set_cell("product_uom_qty", Cell::Decimal(dec("3"))),
                )],
            ),
        )
        .await;
        assert_eq!(
            amount(&mut tx, &quotation, "amount_total").await,
            dec("330")
        );
        let copied = quotation.copy(&mut tx, None).await.unwrap();
        let lines = copied.related(&mut tx, "order_line").await.unwrap();
        assert_eq!(amount(&mut tx, &lines, "price_unit").await, dec("100"));
        assert_eq!(ids_of(&mut tx, &lines, "tax_ids").await, vec![vat]);
        assert_eq!(amount(&mut tx, &copied, "amount_total").await, dec("330"));
        tx.rollback().await.unwrap();
    });
}

#[test]
fn the_read_only_rules_of_the_form_are_server_rules() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let quotation = new_order(&mut tx, &world).await;
        let chair = product(&mut tx, "Rule chair", "100", &[]).await;
        add_line(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        act(&mut tx, &quotation, "action_confirm").await.unwrap();
        for (field, label) in [
            ("validity_date", "Expiration"),
            ("date_order", "Order Date"),
        ] {
            let value = if field == "date_order" {
                Cell::DateTime(DateTime::now())
            } else {
                Cell::Date(Date::from_ymd(2030, 1, 1).unwrap())
            };
            refused(
                write(&mut tx, &quotation, RawVals::new().set_cell(field, value)).await,
                ErrorKind::Validation,
                label,
            );
        }
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("fiscal_position_id", Cell::Null),
            )
            .await,
            ErrorKind::Validation,
            "Fiscal Position",
        );
        // locked: salesperson and team
        act(&mut tx, &quotation, "action_lock").await.unwrap();
        refused(
            write(
                &mut tx,
                &quotation,
                RawVals::new().set_cell("user_id", Cell::Null),
            )
            .await,
            ErrorKind::Validation,
            "is locked",
        );
        tx.rollback().await.unwrap();
    });
}

/// lines; one in the same currency is taken)
#[test]
fn a_pricelist_in_another_currency_is_refused_on_a_quotation_with_lines() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap();
        let home = many2one(&mut tx, &company, "currency_id").await.unwrap();
        let eur = xmlid(&mut tx, "base", "EUR").await;
        let usd = xmlid(&mut tx, "base", "USD").await;
        let foreign = if home == eur { usd } else { eur };
        tx.env()
            .records("res.currency")
            .unwrap()
            .browse([foreign])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await
            .unwrap();
        for (currency, rate) in [(home, "1"), (foreign, "0.9")] {
            create(
                &mut tx,
                "res.currency.rate",
                RawVals::new()
                    .set_cell("name", Cell::Date(Date::from_ymd(2026, 1, 1).unwrap()))
                    .set_cell("rate", Cell::Decimal(dec(rate)))
                    .set_cell("currency_id", Cell::Id(currency))
                    .set_cell("company_id", Cell::Id(world.company)),
            )
            .await;
        }
        let mut lists = Vec::new();
        for (name, currency) in [("Same", home), ("Abroad", foreign)] {
            lists.push(
                create(
                    &mut tx,
                    "product.pricelist",
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("currency_id", Cell::Id(currency)),
                )
                .await,
            );
        }
        let chair = product(&mut tx, "Currency chair", "100", &[]).await;
        let quotation = new_order(&mut tx, &world).await;
        // no lines yet: any pricelist is taken
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("pricelist_id", Cell::Id(lists[1])),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("pricelist_id", Cell::Id(lists[0])),
        )
        .await
        .unwrap();
        let line = add_line(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("100"));
        // switching a draft quotation with lines to a foreign pricelist converts its lines
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("pricelist_id", Cell::Id(lists[1])),
        )
        .await
        .unwrap();
        assert_eq!(
            many2one(&mut tx, &quotation, "currency_id").await,
            Some(foreign)
        );
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("90"));
        write(
            &mut tx,
            &quotation,
            RawVals::new().set_cell("pricelist_id", Cell::Id(lists[0])),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("100"));
        tx.rollback().await.unwrap();
    });
}

#[test]
fn a_quotation_does_not_reopen_its_invoicing() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let quotation = new_order(&mut tx, &world).await;
        refused(
            act(&mut tx, &quotation, "action_reopen_order").await,
            ErrorKind::UserError,
            "Only a confirmed sales order can have its invoicing reopened",
        );
        tx.rollback().await.unwrap();
    });
}

/// right is checked before a number is drawn)
#[test]
fn a_new_quotation_starts_with_the_company_and_create_checks_access_first() {
    run(async {
        let (db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let defaults: erp_sdk::ValueMap = tx
            .env()
            .with_company(world.company)
            .unwrap()
            .records("sale.order")
            .unwrap()
            .call(&mut tx, "default_get", (vec!["company_id".to_owned()],))
            .await
            .unwrap();
        assert_eq!(
            defaults.get("company_id"),
            Some(&erp_sdk::Value::Int(world.company))
        );
        // a plain internal user has no right to create orders: refused before any number is used
        admin(&mut tx).await;
        let internal = xmlid(&mut tx, "base", "group_user").await;
        let partner = partner(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Plain".into())),
        )
        .await;
        let plain = create(
            &mut tx,
            "res.users",
            RawVals::new()
                .set_cell("name", Cell::Str("Plain".into()))
                .set_cell("login", Cell::Str("plain-sale".into()))
                .set_cell("partner_id", Cell::Id(partner))
                .set_cell("company_id", Cell::Id(world.company))
                .set_commands("company_ids", [Command::set([id(world.company)])])
                .set_commands("group_ids", [Command::set([id(internal)])]),
        )
        .await;
        let first = new_order(&mut tx, &world).await;
        let first = text(&mut tx, &first, "name").await;
        tx.commit().await.unwrap();
        let orm = policy_orm(&db, &orm).await;
        let mut tx = orm
            .begin(erp_orm::UserId::new(plain).unwrap())
            .await
            .unwrap();
        let result = tx
            .savepoint(async |tx| {
                tx.env()
                    .records("sale.order")?
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("partner_id", Cell::Id(world.customer))
                                .set_cell("company_id", Cell::Id(world.company)),
                        ],
                    )
                    .await
                    .map(|_| ())
            })
            .await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Access);
        tx.rollback().await.unwrap();
        // the refused create drew no number: the next order follows the first one
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let second = new_order(&mut tx, &world).await;
        let second = text(&mut tx, &second, "name").await;
        let trailing = |name: &str| -> i64 {
            name.trim_start_matches(|c: char| !c.is_ascii_digit())
                .rsplit(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(
            trailing(&second),
            trailing(&first) + 1,
            "{first} then {second}"
        );
        tx.rollback().await.unwrap();
    });
}

/// read the ordered-quantities policy afterwards)
#[test]
fn sale_installs_on_a_database_that_already_has_products() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let initialized = Box::pin(system.initialize(InitOptions::new()))
            .await
            .unwrap();
        assert!(initialized.post_commit.is_clean());
        let first = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("product").unwrap()],
        ))
        .await
        .unwrap();
        let mut tx = first.orm.begin(SUPERUSER_ID).await.unwrap();
        let existing = tx
            .env()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Before sale".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        let second = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("sale").unwrap()],
        ))
        .await
        .unwrap();
        assert!(second.post_commit.is_clean(), "{:?}", second.post_commit);
        let mut tx = second.orm.begin(SUPERUSER_ID).await.unwrap();
        let product = tx
            .env()
            .records("product.product")
            .unwrap()
            .browse([existing])
            .unwrap();
        assert_eq!(text(&mut tx, &product, "invoice_policy").await, "order");
        tx.rollback().await.unwrap();
    });
}

/// context carries one)
#[test]
fn a_new_product_takes_the_company_policy_unless_the_context_gives_one() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        tx.env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("sale_invoice_policy", Cell::Str("delivery".into())),
            )
            .await
            .unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let plain = env
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Plain".into()))
                        .set_cell("type", Cell::Str("service".into())),
                ],
            )
            .await
            .unwrap();
        assert_eq!(text(&mut tx, &plain, "invoice_policy").await, "delivery");
        let chosen = env
            .with_context([("default_invoice_policy", Value::Str("order".into()))])
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Chosen".into()))
                        .set_cell("type", Cell::Str("service".into())),
                ],
            )
            .await
            .unwrap();
        assert_eq!(text(&mut tx, &chosen, "invoice_policy").await, "order");
        tx.rollback().await.unwrap();
    });
}

/// priced; the pricelist minimum is compared at the product unit's rounding)
#[test]
fn a_line_in_a_unit_that_does_not_divide_the_product_unit_is_priced() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let unit = xmlid(&mut tx, "uom", "product_uom_unit").await;
        let pack = xmlid(&mut tx, "uom", "product_uom_pack_6").await;
        let three = create(
            &mut tx,
            "uom.uom",
            RawVals::new()
                .set_cell("name", Cell::Str("Triple".into()))
                .set_cell("relative_factor", Cell::Decimal(dec("3")))
                .set_cell("relative_uom_id", Cell::Id(unit)),
        )
        .await;
        let quotation = new_order(&mut tx, &world).await;
        for (name, product_unit, expected) in [("By six", pack, "100"), ("By three", three, "50")] {
            let item = create(
                &mut tx,
                "product.product",
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell(
                        "list_price",
                        Cell::Decimal(dec(if expected == "100" { "600" } else { "150" })),
                    )
                    .set_cell("uom_id", Cell::Id(product_unit)),
            )
            .await;
            let line = add_line(
                &mut tx,
                &quotation,
                RawVals::new()
                    .set_cell("product_id", Cell::Id(item))
                    .set_cell("product_uom_id", Cell::Id(unit))
                    .set_cell("product_uom_qty", Cell::Decimal(dec("1"))),
            )
            .await;
            assert_eq!(
                amount(&mut tx, &line, "price_unit").await,
                dec(expected),
                "{name}"
            );
        }
        tx.rollback().await.unwrap();
    });
}

/// again; a new pricelist in the same currency prices the other lines again and keeps hand-set
/// prices)
#[test]
fn a_quantity_or_pricelist_change_prices_the_lines_again_except_hand_set_ones() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let chair = product(&mut tx, "Reprice chair", "100", &[]).await;
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap();
        let currency = many2one(&mut tx, &company, "currency_id").await.unwrap();
        let pricelist = create(
            &mut tx,
            "product.pricelist",
            RawVals::new()
                .set_cell("name", Cell::Str("Volume".into()))
                .set_cell("currency_id", Cell::Id(currency)),
        )
        .await;
        create(
            &mut tx,
            "product.pricelist.item",
            RawVals::new()
                .set_cell("pricelist_id", Cell::Id(pricelist))
                .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("min_quantity", Cell::Decimal(dec("10")))
                .set_cell("compute_price", Cell::Str("fixed".into()))
                .set_cell("fixed_price", Cell::Decimal(dec("80"))),
        )
        .await;
        let priced = order(
            &mut tx,
            &world,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await;
        let line = add_line(
            &mut tx,
            &priced,
            RawVals::new().set_cell("product_id", Cell::Id(chair)),
        )
        .await;
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("100"));
        write(
            &mut tx,
            &line,
            RawVals::new().set_cell("product_uom_qty", Cell::Decimal(dec("10"))),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &line, "price_unit").await, dec("80"));

        // a pricelist given later: the automatic line follows it, the hand-set one stays
        let plain = new_order(&mut tx, &world).await;
        let automatic = add_line(
            &mut tx,
            &plain,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("product_uom_qty", Cell::Decimal(dec("10"))),
        )
        .await;
        let manual = add_line(
            &mut tx,
            &plain,
            RawVals::new()
                .set_cell("product_id", Cell::Id(chair))
                .set_cell("product_uom_qty", Cell::Decimal(dec("10"))),
        )
        .await;
        write(
            &mut tx,
            &manual,
            RawVals::new().set_cell("price_unit", Cell::Decimal(dec("50"))),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &plain,
            RawVals::new().set_cell("pricelist_id", Cell::Id(pricelist)),
        )
        .await
        .unwrap();
        assert_eq!(amount(&mut tx, &automatic, "price_unit").await, dec("80"));
        assert_eq!(amount(&mut tx, &manual, "price_unit").await, dec("50"));
        tx.rollback().await.unwrap();
    });
}
