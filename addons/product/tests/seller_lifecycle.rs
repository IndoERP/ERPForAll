// SPDX-License-Identifier: MIT OR Apache-2.0
//! Supplier defaults, normalized prices and write invariants.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, ValueMap};

async fn fixture() -> (erp_testkit::Harness, i64) {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Vendor terms").await;
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Unit".into()))
                    .set_cell("digits", Cell::Int(3)),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (h, company)
}

async fn catalog(tx: &mut Transaction, env: &Env) -> (i64, i64, i64) {
    let vendor = env
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Vendor Å".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    let template = env
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Part".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    let variant = env
        .records("product.product")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, template).unwrap(),
        )
        .await
        .unwrap()
        .ids()[0];
    (vendor, template, variant)
}

#[tokio::test]
async fn supplier_defaults_and_explicit_values() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, _) = catalog(&mut tx, &env).await;
    let suppliers = env.records("product.supplierinfo").unwrap();
    let defaults = suppliers
        .default_get(
            &mut tx,
            &["company_id", "currency_id", "min_qty", "price", "delay"],
        )
        .await
        .unwrap();
    assert_eq!(defaults.get("company_id"), Some(&Value::Int(company)));
    let row = suppliers
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(company)
    );
    assert_eq!(
        row.cell(&mut tx, "currency_id").await.unwrap(),
        env.records("res.company")
            .unwrap()
            .browse([company])
            .unwrap()
            .cell(&mut tx, "currency_id")
            .await
            .unwrap()
    );
    assert_eq!(
        row.cell(&mut tx, "uom_id").await.unwrap(),
        Cell::Id(support::unit_id(&h).await)
    );
    assert_eq!(
        row.cell(&mut tx, "min_qty").await.unwrap(),
        Cell::Decimal(Decimal::ONE)
    );
    assert_eq!(
        row.cell(&mut tx, "price").await.unwrap(),
        Cell::Decimal(Decimal::ZERO)
    );
    assert_eq!(row.cell(&mut tx, "delay").await.unwrap(), Cell::Int(1));
    assert_eq!(
        row.cell(&mut tx, "product_variant_count").await.unwrap(),
        Cell::Int(1)
    );
    assert_eq!(
        row.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("Vendor Å".into())
    );
    let explicit = suppliers
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("company_id", Cell::Null)
                    .set_cell("min_qty", Cell::Decimal(Decimal::new(1234, 3)))
                    .set_cell("price", Cell::Decimal(Decimal::new(1250, 2))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        explicit.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        explicit.cell(&mut tx, "min_qty").await.unwrap(),
        Cell::Decimal(Decimal::new(1234, 3))
    );
}

#[tokio::test]
async fn variant_only_values_are_sanitized_and_results_are_complete() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, variant) = catalog(&mut tx, &env).await;
    let suppliers = env.records("product.supplierinfo").unwrap();
    let row = suppliers
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_id", Cell::Id(variant)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.cell(&mut tx, "product_tmpl_id").await.unwrap(),
        Cell::Id(template)
    );
    let info: ValueMap = row.call(&mut tx, "_get_seller_info", ()).await.unwrap();
    assert_eq!(info.len(), 9);
    assert_eq!(info.get("supplierinfo"), Some(&Value::Int(row.ids()[0])));
    assert_eq!(info.get("partner_id"), Some(&Value::Int(vendor)));
    row.write(&mut tx, RawVals::new().set_cell("product_id", Cell::Null))
        .await
        .unwrap();
    assert_eq!(
        row.cell(&mut tx, "product_tmpl_id").await.unwrap(),
        Cell::Id(template)
    );
    row.write(
        &mut tx,
        RawVals::new()
            .set_cell("product_id", Cell::Id(variant))
            .set_cell("product_tmpl_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(
        row.cell(&mut tx, "product_tmpl_id").await.unwrap(),
        Cell::Id(template)
    );
    let empty: ValueMap = suppliers
        .call(&mut tx, "_get_seller_info", ())
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn discounted_price_normalizes_units_and_cost_is_explicit() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, variant) = catalog(&mut tx, &env).await;
    let unit = support::unit_id(&h).await;
    let dozen = env
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Dozen".into()))
                    .set_cell("relative_uom_id", Cell::Id(unit))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(12))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let row = env
        .records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("product_id", Cell::Id(variant))
                    .set_cell("price", Cell::Decimal(Decimal::from(100)))
                    .set_cell("discount", Cell::Decimal(Decimal::from(15))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        row.cell(&mut tx, "price_discounted").await.unwrap(),
        Cell::Decimal(Decimal::from(85))
    );
    row.write(
        &mut tx,
        RawVals::new()
            .set_cell("uom_id", Cell::Id(dozen))
            .set_cell("price", Cell::Decimal(Decimal::from(120))),
    )
    .await
    .unwrap();
    assert_eq!(
        row.cell(&mut tx, "price_discounted").await.unwrap(),
        Cell::Decimal(Decimal::new(85, 1))
    );
    let probed = row.with_context([("test_cost_probe", Value::Int(4250))]);
    let _: () = probed.call(&mut tx, "_compute_price", ()).await.unwrap();
    assert_eq!(
        row.cell(&mut tx, "price").await.unwrap(),
        Cell::Decimal(Decimal::new(4250, 2))
    );
    assert_eq!(row.cell(&mut tx, "uom_id").await.unwrap(), Cell::Id(dozen));
}

#[tokio::test]
async fn explicit_mismatched_template_is_refused_on_write() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, variant) = catalog(&mut tx, &env).await;
    let (_, other, _) = catalog(&mut tx, &env).await;
    let row = env
        .records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_id", Cell::Id(variant)),
            ],
        )
        .await
        .unwrap();
    let err = row
        .write(
            &mut tx,
            RawVals::new().set_cell("product_tmpl_id", Cell::Id(other)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        format!("Supplierinfo variant {variant} does not belong to product template {other}.")
    );
    assert_eq!(
        row.cell(&mut tx, "product_tmpl_id").await.unwrap(),
        Cell::Id(template)
    );
}

#[tokio::test]
async fn recurring_prices_round_for_results_and_remain_exact_natively() {
    let (h, company) = fixture().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let precision = env
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Price".into()))
                    .set_cell("digits", Cell::Int(3)),
            ],
        )
        .await
        .unwrap();
    let (vendor, template, _) = catalog(&mut tx, &env).await;
    let dozen = env
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Dozen".into()))
                    .set_cell("relative_uom_id", Cell::Id(unit))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(12))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let row = env
        .records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("uom_id", Cell::Id(dozen))
                    .set_cell("price", Cell::Decimal(Decimal::ONE)),
            ],
        )
        .await
        .unwrap();
    let exact: erp_sdk::money::ExactRatio = row
        .call(&mut tx, "_get_discounted_price", ())
        .await
        .unwrap();
    assert_eq!(
        exact,
        erp_sdk::money::ExactRatio::quotient(Decimal::ONE, Decimal::from(12)).unwrap()
    );
    assert_eq!(
        row.cell(&mut tx, "price_discounted").await.unwrap(),
        Cell::Decimal(Decimal::new(83, 3))
    );
    precision
        .write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(4)))
        .await
        .unwrap();
    let native: Option<erp_addon_product::SellerInfo> = row
        .call(&mut tx, "_get_seller_info_exact", ())
        .await
        .unwrap();
    let native = native.unwrap();
    assert_eq!(native.discounted_price(), &exact);
    assert_eq!(
        native.values().get("price_discounted"),
        Some(&Value::Decimal(Decimal::new(833, 4)))
    );
}

#[tokio::test]
async fn context_product_defaults_and_product_changes_preserve_the_vendor_unit() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, variant) = catalog(&mut tx, &env).await;
    let model = env
        .records("product.supplierinfo")
        .unwrap()
        .with_context([("default_product_id", Value::Int(variant))]);
    let defaults = model
        .default_get(&mut tx, &["product_id", "product_tmpl_id", "uom_id"])
        .await
        .unwrap();
    assert_eq!(defaults.get("product_tmpl_id"), Some(&Value::Int(template)));
    assert_eq!(
        defaults.get("uom_id"),
        Some(&Value::Int(support::unit_id(&h).await))
    );
    let row = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("partner_id", Cell::Id(vendor))],
        )
        .await
        .unwrap();
    let (_, other, _) = catalog(&mut tx, &env).await;
    let unit = row.cell(&mut tx, "uom_id").await.unwrap();
    row.write(
        &mut tx,
        RawVals::new()
            .set_cell("product_id", Cell::Null)
            .set_cell("product_tmpl_id", Cell::Id(other)),
    )
    .await
    .unwrap();
    assert_eq!(row.cell(&mut tx, "uom_id").await.unwrap(), unit);
}

#[tokio::test]
async fn incompatible_vendor_units_name_the_agreement_in_the_error() {
    let (h, company) = fixture().await;
    let target = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, _) = catalog(&mut tx, &env).await;
    let source = env
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Separate dimension".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    let row = env
        .records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("uom_id", Cell::Id(source)),
            ],
        )
        .await
        .unwrap();
    let error = row
        .call::<_, erp_sdk::money::ExactRatio>(&mut tx, "_get_discounted_price", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Vendor agreement {}: Units of measure {source} and {target} do not share a reference root.",
            row.ids()[0]
        )
    );
}

#[tokio::test]
async fn overflowing_discounted_results_return_a_named_validation_error() {
    let (h, company) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, template, _) = catalog(&mut tx, &env).await;
    let row = env
        .records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(vendor))
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("price", Cell::Decimal(Decimal::MAX))
                    .set_cell("discount", Cell::Decimal(Decimal::from(-100))),
            ],
        )
        .await
        .unwrap();
    let error = row.cell(&mut tx, "price_discounted").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Vendor agreement {}: The monetary value is out of the supported range (quantize).",
            row.ids()[0]
        )
    );
}

#[tokio::test]
async fn missing_company_has_no_currency_fallback() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("product.supplierinfo").unwrap();
    let error = model
        .default_get(&mut tx, &["currency_id"])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "No company is active, so the default vendor currency cannot be determined: set the company of the record or run in an environment that has a company."
    );
}
