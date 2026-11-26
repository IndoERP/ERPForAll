// SPDX-License-Identifier: MIT OR Apache-2.0
//! Live pre-save `onchange` for `purchase.order.line` and `purchase.order`: product selection,
//! quantity/price/discount edits, and header total computation without touching the database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::sync::Arc;

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
        &erp_addon_reporting::ADDON,
        &erp_addon_purchase::ADDON,
    ])
    .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
    .before_schema(
        "CREATE TABLE IF NOT EXISTS ir_model_data (\
         id bigserial PRIMARY KEY, \
         module text NOT NULL, \
         name text NOT NULL, \
         model text NOT NULL, \
         res_id int8 NOT NULL, \
         noupdate boolean NOT NULL DEFAULT false\
         )",
    )
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap()
}

fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

fn obj(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = erp_sdk::ValueMap::new();
    for (k, v) in entries {
        map.insert(k.to_owned(), v);
    }
    Value::Map(Arc::new(map))
}

#[tokio::test]
async fn live_onchange_computes_purchase_line_and_order_totals_before_save() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for (module, name, label) in [
        ("base", "group_user", "Internal User"),
        ("base", "group_system", "Settings"),
        ("base", "group_erp_manager", "Access Rights"),
        ("account", "group_account_readonly", "Accounting Readonly"),
        ("account", "group_account_user", "Accounting User"),
        ("account", "group_account_manager", "Accounting Manager"),
        (
            "account",
            "group_cost_center_controlling",
            "Cost Center Controlling",
        ),
        ("account", "group_asset_cip", "Asset & CIP"),
        ("account", "group_budget_avc", "Budget AVC"),
        (
            "account",
            "group_group_consolidation",
            "Group Consolidation",
        ),
        ("purchase", "group_purchase_user", "Purchase User"),
        ("purchase", "group_purchase_manager", "Purchase Manager"),
    ] {
        let gid: i64 = tx
            .query(
                "INSERT INTO res_groups (name) VALUES (jsonb_build_object('en_US', $1::text)) RETURNING id::int8",
                &[&label],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.execute(
            "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
             VALUES ($1, $2, 'res.groups', $3, true)",
            &[&module, &name, &gid],
        )
        .await
        .unwrap();
    }

    // Seed currency, company, UoM, vendor, purchase tax and product.
    let currency: i64 = match tx
        .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .first()
    {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency_in(&mut tx)
            .await
            .unwrap(),
    };
    let company = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Buyer Co".into()))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let uom: i64 = tx
        .query(
            "INSERT INTO uom_uom (name, active, relative_factor, factor, sequence) \
             VALUES (jsonb_build_object('en_US', 'Units'), true, 1, 1, 1) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "UPDATE uom_uom SET parent_path = $2 WHERE id = $1",
        &[&uom, &format!("{uom}/")],
    )
    .await
    .unwrap();
    let vendor = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Acme Vendor".into()))],
        )
        .await
        .unwrap()
        .ids()[0];

    let country: i64 = tx
        .env()
        .records("res.country")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Testland".into()))
                    .set_cell("code", Cell::Str("PT".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let tax_group: i64 = tx
        .query(
            "INSERT INTO account_tax_group (name, sequence, company_id) \
             VALUES (jsonb_build_object('en_US', 'VAT'), 10, $1) RETURNING id::int8",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    let vat10: i64 = tx
        .query(
            "INSERT INTO account_tax (name, type_tax_use, amount_type, active, company_id, \
             root_company_id, sequence, amount, include_base_amount, is_base_affected, \
             tax_group_id, country_id, repartition_serial, analytic) \
             VALUES (jsonb_build_object('en_US', 'Purchase VAT 10%'), 'purchase', 'percent', true, \
             $1, $1, 1, 10, false, true, $2, $3, 0, false) RETURNING id::int8",
            &[&company, &tax_group, &country],
        )
        .await
        .unwrap()[0]
        .get(0);

    let env = tx.env().with_company(company).unwrap();
    let tmpl: i64 = tx
        .query(
            "INSERT INTO product_template (name, active, type, uom_id, list_price) \
             VALUES (jsonb_build_object('en_US', 'Steel Bolt'), true, 'consu', $1, 50) \
             RETURNING id::int8",
            &[&uom],
        )
        .await
        .unwrap()[0]
        .get(0);
    let product: i64 = tx
        .query(
            "INSERT INTO product_product (product_tmpl_id, active, standard_price, lst_price) \
             VALUES ($1, true, 50, 50) RETURNING id::int8",
            &[&tmpl],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "INSERT INTO product_supplier_taxes_rel (prod_id, tax_id) VALUES ($1, $2)",
        &[&tmpl, &vat10],
    )
    .await
    .unwrap();

    // 1. Line onchange on an unsaved line with a virtual unsaved `purchase.order` parent.
    let line_spec = obj([
        ("name", obj([])),
        ("uom_id", obj([])),
        ("price_unit", obj([])),
        ("technical_price_unit", obj([])),
        ("discount", obj([])),
        ("tax_ids", obj([])),
        ("date_planned", obj([])),
        ("price_subtotal", obj([])),
        ("price_tax", obj([])),
        ("price_total", obj([])),
    ]);
    let parent_dict = obj([
        ("id", Value::False),
        ("company_id", Value::Int(company)),
        ("currency_id", Value::Int(currency)),
        ("partner_id", Value::Int(vendor)),
    ]);

    let res = env
        .records("purchase.order.line")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj([
                (
                    "values",
                    obj([
                        ("order_id", parent_dict.clone()),
                        ("product_id", Value::Int(product)),
                        ("product_qty", Value::Decimal(dec("4"))),
                    ]),
                ),
                ("field_names", Value::list([Value::str("product_id")])),
                ("fields_spec", line_spec.clone()),
            ]),
        )
        .await
        .unwrap();
    let vals = res.dict().unwrap().get("value").unwrap();
    let map = vals.dict().unwrap();
    assert_eq!(map.get("uom_id").unwrap(), Value::Int(uom));
    assert_eq!(map.get("price_unit").unwrap(), Value::Decimal(dec("50")));
    assert_eq!(
        map.get("price_subtotal").unwrap(),
        Value::Decimal(dec("200"))
    );
    assert_eq!(map.get("price_tax").unwrap(), Value::Decimal(dec("20")));
    assert_eq!(map.get("price_total").unwrap(), Value::Decimal(dec("220")));

    // Editing price_unit + discount updates line amounts immediately.
    let res = env
        .records("purchase.order.line")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj([
                (
                    "values",
                    obj([
                        ("order_id", parent_dict),
                        ("product_id", Value::Int(product)),
                        ("uom_id", Value::Int(uom)),
                        ("product_qty", Value::Decimal(dec("4"))),
                        ("price_unit", Value::Decimal(dec("100"))),
                        ("technical_price_unit", Value::Decimal(dec("50"))),
                        ("discount", Value::Decimal(dec("25"))),
                        (
                            "tax_ids",
                            Value::list([Value::list([
                                Value::Int(6),
                                Value::Int(0),
                                Value::list([Value::Int(vat10)]),
                            ])]),
                        ),
                        ("price_subtotal", Value::Decimal(dec("200"))),
                        ("price_tax", Value::Decimal(dec("20"))),
                        ("price_total", Value::Decimal(dec("220"))),
                    ]),
                ),
                (
                    "field_names",
                    Value::list([Value::str("price_unit"), Value::str("discount")]),
                ),
                ("fields_spec", line_spec),
            ]),
        )
        .await
        .unwrap();
    let vals = res.dict().unwrap().get("value").unwrap();
    let map = vals.dict().unwrap();
    // 4 * 100 * 0.75 = 300 subtotal, 30 tax, 330 total
    assert_eq!(
        map.get("price_subtotal").unwrap(),
        Value::Decimal(dec("300"))
    );
    assert_eq!(map.get("price_tax").unwrap(), Value::Decimal(dec("30")));
    assert_eq!(map.get("price_total").unwrap(), Value::Decimal(dec("330")));

    // 2. Order onchange computes header totals before Save on an unsaved RFQ.
    let order_spec = obj([
        ("amount_untaxed", obj([])),
        ("amount_tax", obj([])),
        ("amount_total", obj([])),
        ("amount_total_cc", obj([])),
        (
            "order_line",
            obj([(
                "fields",
                obj([
                    ("price_subtotal", obj([])),
                    ("price_tax", obj([])),
                    ("price_total", obj([])),
                ]),
            )]),
        ),
    ]);
    let order_res = env
        .records("purchase.order")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj([
                (
                    "values",
                    obj([
                        ("partner_id", Value::Int(vendor)),
                        ("company_id", Value::Int(company)),
                        ("currency_id", Value::Int(currency)),
                        ("amount_untaxed", Value::Decimal(Decimal::ZERO)),
                        ("amount_tax", Value::Decimal(Decimal::ZERO)),
                        ("amount_total", Value::Decimal(Decimal::ZERO)),
                        ("amount_total_cc", Value::Decimal(Decimal::ZERO)),
                        (
                            "order_line",
                            Value::list([Value::list([
                                Value::Int(0),
                                Value::str("line_1"),
                                obj([
                                    ("product_id", Value::Int(product)),
                                    ("name", Value::str("Steel Bolt")),
                                    ("uom_id", Value::Int(uom)),
                                    ("product_qty", Value::Decimal(dec("5"))),
                                    ("price_unit", Value::Decimal(dec("50"))),
                                    ("discount", Value::Decimal(Decimal::ZERO)),
                                    (
                                        "tax_ids",
                                        Value::list([Value::list([
                                            Value::Int(6),
                                            Value::Int(0),
                                            Value::list([Value::Int(vat10)]),
                                        ])]),
                                    ),
                                ]),
                            ])]),
                        ),
                    ]),
                ),
                ("field_names", Value::list([Value::str("order_line")])),
                ("fields_spec", order_spec),
            ]),
        )
        .await
        .unwrap();
    let order_vals = order_res.dict().unwrap().get("value").unwrap();
    let omap = order_vals.dict().unwrap();
    assert_eq!(
        omap.get("amount_untaxed").unwrap(),
        Value::Decimal(dec("250"))
    );
    assert_eq!(omap.get("amount_tax").unwrap(), Value::Decimal(dec("25")));
    assert_eq!(
        omap.get("amount_total").unwrap(),
        Value::Decimal(dec("275"))
    );
    assert_eq!(
        omap.get("amount_total_cc").unwrap(),
        Value::Decimal(dec("275"))
    );

    // Derived fields are still refused on real persistence writes.
    let err = tx
        .savepoint(async |tx| {
            env.records("purchase.order")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("partner_id", Cell::Id(vendor))
                            .set_cell("amount_total", Cell::Decimal(dec("999"))),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
