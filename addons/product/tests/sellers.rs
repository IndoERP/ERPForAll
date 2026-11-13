// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native vendor eligibility and price selection.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_addon_product::SellerQuery;
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, ValueMap};

async fn catalog(tx: &mut Transaction, env: &Env) -> (i64, Records) {
    let vendor = env
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Vendor".into()))],
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
    let product = env
        .records("product.product")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, template).unwrap(),
        )
        .await
        .unwrap();
    (vendor, product)
}

async fn agreement(tx: &mut Transaction, product: &Records, vendor: i64, changes: RawVals) -> i64 {
    let mut values = RawVals::new()
        .set_cell("partner_id", Cell::Id(vendor))
        .set_cell("product_id", Cell::Id(product.ids()[0]));
    for (name, value) in changes.iter() {
        values.insert(name.to_owned().into(), value.clone());
    }
    product
        .env()
        .records("product.supplierinfo")
        .unwrap()
        .create(tx, vec![values])
        .await
        .unwrap()
        .ids()[0]
}

async fn filtered(tx: &mut Transaction, product: &Records, query: SellerQuery) -> Vec<i64> {
    product
        .call::<_, Records>(tx, "_get_filtered_sellers", (query,))
        .await
        .unwrap()
        .ids()
}

async fn selected(tx: &mut Transaction, product: &Records, query: SellerQuery) -> ValueMap {
    product.call(tx, "_select_seller", (query,)).await.unwrap()
}

#[tokio::test]
async fn seller_quantity_dates_and_units_are_filtered() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Selection").await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, product) = catalog(&mut tx, &env).await;
    let pack = env
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Pack".into()))
                    .set_cell("relative_uom_id", Cell::Id(unit))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(12))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let day = Date::from_ymd(2026, 11, 13).unwrap();
    let base = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("date_start", Cell::Date(day))
            .set_cell("date_end", Cell::Date(day)),
    )
    .await;
    let tier = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("uom_id", Cell::Id(pack))
            .set_cell("min_qty", Cell::Decimal(Decimal::new(101, 2))),
    )
    .await;
    let query = SellerQuery::default()
        .with_date(day)
        .with_unit(unit)
        .unwrap();
    assert_eq!(
        filtered(
            &mut tx,
            &product,
            query.clone().with_quantity(Some(Decimal::from(12)))
        )
        .await,
        vec![base]
    );
    // 12.01 units -> 1.00083 packs, rounded UP to 1.01 before comparing.
    assert_eq!(
        filtered(
            &mut tx,
            &product,
            query.clone().with_quantity(Some(Decimal::new(1201, 2)))
        )
        .await,
        vec![tier, base]
    );
    assert!(
        filtered(
            &mut tx,
            &product,
            query.clone().with_quantity(Some(Decimal::ZERO))
        )
        .await
        .is_empty()
    );
    assert_eq!(
        filtered(&mut tx, &product, query.clone().with_quantity(None)).await,
        vec![tier, base]
    );
    assert_eq!(
        filtered(
            &mut tx,
            &product,
            query.clone().with_quantity(None).with_force_uom(true)
        )
        .await,
        vec![base]
    );
    assert_eq!(
        filtered(
            &mut tx,
            &product,
            query
                .with_quantity(None)
                .with_date(Date::from_ymd(2026, 11, 14).unwrap())
        )
        .await,
        vec![tier]
    );
}

#[tokio::test]
async fn seller_partner_and_variant_filters_are_explicit() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Partner selection").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, product) = catalog(&mut tx, &env).await;
    let child = env
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Vendor contact".into()))
                    .set_cell("parent_id", Cell::Id(vendor)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let (other, _) = catalog(&mut tx, &env).await;
    let matching = agreement(&mut tx, &product, vendor, RawVals::new()).await;
    let other_vendor = agreement(&mut tx, &product, other, RawVals::new()).await;
    let template = product.cell(&mut tx, "product_tmpl_id").await.unwrap();
    let other_variant = env
        .records("product.product")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", template)
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    agreement(&mut tx, &other_variant, vendor, RawVals::new()).await;
    let prepared: Records = product
        .call(&mut tx, "_prepare_sellers", (SellerQuery::default(),))
        .await
        .unwrap();
    assert_eq!(
        prepared.ids(),
        vec![matching, other_vendor],
        "preparation excludes a different variant of this same template"
    );
    let query = SellerQuery::default()
        .with_quantity(None)
        .with_partner(child)
        .unwrap();
    assert_eq!(
        filtered(&mut tx, &product, query.clone()).await,
        vec![matching]
    );
    assert!(
        filtered(
            &mut tx,
            &product,
            query.with_parameter_partner(child).unwrap()
        )
        .await
        .is_empty()
    );
    env.records("res.partner")
        .unwrap()
        .browse([vendor])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert!(
        filtered(
            &mut tx,
            &product,
            SellerQuery::default()
                .with_quantity(None)
                .with_partner(child)
                .unwrap()
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn first_vendor_and_discounted_price_determine_selection() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Priority").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, product) = catalog(&mut tx, &env).await;
    let (other, _) = catalog(&mut tx, &env).await;
    let expensive = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("price", Cell::Decimal(Decimal::from(100)))
            .set_cell("delay", Cell::Int(1)),
    )
    .await;
    let discounted = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("price", Cell::Decimal(Decimal::from(120)))
            .set_cell("discount", Cell::Decimal(Decimal::from(50)))
            .set_cell("delay", Cell::Int(5)),
    )
    .await;
    agreement(
        &mut tx,
        &product,
        other,
        RawVals::new()
            .set_cell("sequence", Cell::Int(2))
            .set_cell("price", Cell::Decimal(Decimal::ONE)),
    )
    .await;
    let query = SellerQuery::default().with_quantity(Some(Decimal::ONE));
    assert_eq!(
        selected(&mut tx, &product, query.clone())
            .await
            .get("supplierinfo"),
        Some(&Value::Int(discounted))
    );
    assert_eq!(
        selected(
            &mut tx,
            &product,
            query.clone().with_order("delay").unwrap()
        )
        .await
        .get("supplierinfo"),
        Some(&Value::Int(expensive))
    );
    let err = product
        .call::<_, ValueMap>(
            &mut tx,
            "_select_seller",
            (query.with_order("missing_field").unwrap(),),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.to_string(),
        "Unknown seller ordering field 'missing_field' on product.supplierinfo."
    );
}

#[tokio::test]
async fn prices_are_compared_in_company_currency_without_rounding() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "FX selection").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let rate_type = env
        .records("res.currency.rate.type")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Spot".into()))
                    .set_cell("code", Cell::Str("spot".into()))
                    .set_cell("date_rule", Cell::Str("strictly_before".into()))
                    .set_cell("later_fallback", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let currency = env
        .records("res.currency")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("EUR".into()))
                    .set_cell("symbol", Cell::Str("€".into()))
                    .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    env.records("res.currency.rate")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(currency))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("rate_type_id", Cell::Id(rate_type))
                    .set_cell("name", Cell::Date(Date::from_ymd(2026, 11, 12).unwrap()))
                    .set_cell("rate", Cell::Decimal(Decimal::from(2))),
            ],
        )
        .await
        .unwrap();
    env.records("decimal.precision")
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
    let (vendor, product) = catalog(&mut tx, &env).await;
    agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new().set_cell("price", Cell::Decimal(Decimal::new(25, 3))),
    )
    .await;
    let cheap = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("currency_id", Cell::Id(currency))
            .set_cell("price", Cell::Decimal(Decimal::new(44, 3))),
    )
    .await;
    let result = selected(
        &mut tx,
        &product,
        SellerQuery::default()
            .with_quantity(Some(Decimal::ONE))
            .with_date(Date::from_ymd(2026, 11, 13).unwrap()),
    )
    .await;
    assert_eq!(result.get("supplierinfo"), Some(&Value::Int(cheap)));
    assert_eq!(
        result.get("price"),
        Some(&Value::Decimal(Decimal::new(44, 3)))
    );
    assert_eq!(result.get("currency_id"), Some(&Value::Int(currency)));
}

#[tokio::test]
async fn readable_ancestor_agreements_are_not_eligible_for_a_branch() {
    let h = support::base_harness().await;
    let root = support::seed_company(&h, "Root").await;
    let other = support::seed_company(&h, "Unrelated").await;
    let mut tx = h.begin().await.unwrap();
    let root_env = tx.env().with_company(root).unwrap();
    let currency = root_env
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let branch = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Branch".into()))
                    .set_cell("parent_id", Cell::Id(root))
                    .set_cell("currency_id", currency),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let (vendor, product) = catalog(&mut tx, &root_env).await;
    agreement(&mut tx, &product, vendor, RawVals::new()).await;
    agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new().set_cell("company_id", Cell::Id(other)),
    )
    .await;
    let global = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new().set_cell("company_id", Cell::Null),
    )
    .await;
    let branch_product = tx
        .env()
        .with_company(branch)
        .unwrap()
        .records("product.product")
        .unwrap()
        .browse(product.ids())
        .unwrap();
    let local = agreement(&mut tx, &branch_product, vendor, RawVals::new()).await;
    assert_eq!(
        filtered(
            &mut tx,
            &branch_product,
            SellerQuery::default().with_quantity(None)
        )
        .await,
        vec![global, local]
    );
}

#[tokio::test]
async fn later_addons_can_extend_preparation_and_exact_results_survive_rounding() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Extensions").await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let (vendor, product) = catalog(&mut tx, &env).await;
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
    let first = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("uom_id", Cell::Id(dozen))
            .set_cell("price", Cell::Decimal(Decimal::ONE)),
    )
    .await;
    let second = agreement(
        &mut tx,
        &product,
        vendor,
        RawVals::new()
            .set_cell("uom_id", Cell::Id(dozen))
            .set_cell("price", Cell::Decimal(Decimal::new(99, 2))),
    )
    .await;
    let query = SellerQuery::default().with_quantity(None);
    let result = selected(&mut tx, &product, query.clone()).await;
    assert_eq!(result.get("supplierinfo"), Some(&Value::Int(second)));
    assert_eq!(
        result.get("price_discounted"),
        Some(&Value::Decimal(Decimal::new(8, 2)))
    );
    assert_ne!(first, second);
    let probed = product.with_context([("test_seller_probe", Value::Bool(true))]);
    assert!(selected(&mut tx, &probed, query).await.is_empty());
}

#[tokio::test]
async fn query_rejects_invalid_identities_and_order_expressions() {
    for id in [0, -1] {
        let error = SellerQuery::default().with_partner(id).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), "Seller partner identity must be positive.");
    }
    let error = SellerQuery::default().with_order("price desc").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Seller ordering must name one scalar supplier field."
    );
}
