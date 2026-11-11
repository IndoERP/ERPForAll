//! Pricelist and Pricing Engine tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_product::ProductPricelist;
mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = support::base_harness().await;
    support::seed_company(&h, "Pricelist Company").await;
    h
}

#[tokio::test]
async fn test_pricelist_currency_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Default Currency PL".into()))],
        )
        .await
        .unwrap();

    let curr_cell = pl.cell(&mut tx, "currency_id").await.unwrap();
    assert!(
        matches!(curr_cell, Cell::Id(id) if id > 0),
        "Expected valid currency_id defaulted on pricelist"
    );
}

#[tokio::test]
async fn test_pricelist_item_precedence_sorting() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Sort PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    // Create rules out of order: Global, Category, Template, Variant
    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("3_global".into()))
                    .set_cell("min_quantity", Cell::Decimal(Decimal::ONE)),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("2_product_category".into()))
                    .set_cell("min_quantity", Cell::Decimal(Decimal::ONE)),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                    .set_cell("min_quantity", Cell::Decimal(Decimal::ONE)),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("1_product".into()))
                    .set_cell("min_quantity", Cell::Decimal(Decimal::ONE)),
            ],
        )
        .await
        .unwrap();

    let d = Domain::condition("pricelist_id", Operator::Eq, pl_id).unwrap();
    let items = item_model.search(&mut tx, &d).await.unwrap();
    assert_eq!(items.len(), 4);

    let mut applied_ons = Vec::new();
    for id in items.rec_ids() {
        let rec = item_model.browse(vec![id.get()]).unwrap();
        if let Ok(Cell::Str(s)) = rec.cell(&mut tx, "applied_on").await {
            applied_ons.push(s.to_string());
        }
    }

    assert_eq!(
        applied_ons,
        vec![
            "0_product_variant",
            "1_product",
            "2_product_category",
            "3_global"
        ],
        "Expected items sorted by applied_on tier order"
    );
}

#[tokio::test]
async fn test_pricelist_rule_variant_tier() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("T1".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(50))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant = prod_model.search(&mut tx, &d_prod).await.unwrap();
    let variant_id = variant.rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("PL1".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    // R1 on Variant: 15.0; R2 on Template: 20.0
    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("1_product".into()))
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("compute_price", Cell::Str("fixed".into()))
                    .set_cell("fixed_price", Cell::Decimal(Decimal::from(20))),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("0_product_variant".into()))
                    .set_cell("product_id", Cell::Id(variant_id))
                    .set_cell("compute_price", Cell::Str("fixed".into()))
                    .set_cell("fixed_price", Cell::Decimal(Decimal::from(15))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::from(15),
        "Variant-specific rule must win over template rule"
    );
}

#[tokio::test]
async fn test_pricelist_rule_template_tier() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("T2".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(100))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("PL2".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    // Template rule: fixed 45.0; Global rule: discount 10% (would be 90.0)
    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("3_global".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(10))),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("1_product".into()))
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("compute_price", Cell::Str("fixed".into()))
                    .set_cell("fixed_price", Cell::Decimal(Decimal::from(45))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::from(45),
        "Template-specific rule must win over global rule"
    );
}

#[tokio::test]
async fn test_pricelist_rule_category_tier() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cat_model = support::company_env(&mut tx)
        .await
        .records("product.category")
        .unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let cat = cat_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Chairs".into()))],
        )
        .await
        .unwrap();
    let cat_id = cat.rec_ids()[0].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Office Chair".into()))
                    .set_cell("categ_id", Cell::Id(cat_id))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(100))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("PL3".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    // Category rule: discount 15%; Global rule: discount 5%
    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("3_global".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(5))),
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("2_product_category".into()))
                    .set_cell("categ_id", Cell::Id(cat_id))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(15))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::from(85),
        "Category rule (15% off 100 = 85) must win over global rule"
    );
}

#[tokio::test]
async fn test_pricelist_rule_global_tier() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Desk".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(100))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Global PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    // Global rule: discount 10%
    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("applied_on", Cell::Str("3_global".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(10))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(price, Decimal::from(90), "Global rule (10% off 100 = 90)");
}

#[tokio::test]
async fn test_pricelist_computation_fixed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Item".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(100))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Fixed PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("compute_price", Cell::Str("fixed".into()))
                    .set_cell("fixed_price", Cell::Decimal(Decimal::new(2999, 2))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::new(2999, 2),
        "Fixed price should evaluate exactly to 29.99"
    );
}

#[tokio::test]
async fn test_pricelist_computation_discount() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Item200".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(200))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Discount PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("base", Cell::Str("list_price".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(20))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::from(160),
        "20% discount on 200 should evaluate to 160"
    );
}

#[tokio::test]
async fn test_pricelist_computation_markup() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("CostItem".into()))
                    .set_cell("standard_price", Cell::Decimal(Decimal::from(50))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Markup PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("base", Cell::Str("standard_price".into()))
                    .set_cell("compute_price", Cell::Str("markup".into()))
                    .set_cell("price_markup", Cell::Decimal(Decimal::from(25))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::new(625, 1),
        "25% markup on cost 50 should evaluate to 62.5"
    );
}

#[tokio::test]
async fn test_pricelist_computation_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let pl_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist")
        .unwrap();
    let item_model = support::company_env(&mut tx)
        .await
        .records("product.pricelist.item")
        .unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("RoundItem".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(100))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant_id = prod_model.search(&mut tx, &d_prod).await.unwrap().rec_ids()[0].get();

    let pl = pl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Round PL".into()))],
        )
        .await
        .unwrap();
    let pl_id = pl.rec_ids()[0].get();

    item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl_id))
                    .set_cell("base", Cell::Str("list_price".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::from(15)))
                    .set_cell("price_round", Cell::Decimal(Decimal::from(10))),
            ],
        )
        .await
        .unwrap();

    let pl_set = RecordSet::<ProductPricelist>::try_from(pl).unwrap();
    let price =
        ProductPricelist::get_product_price(&pl_set, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(
        price,
        Decimal::from(90),
        "85.0 rounded to multiple of 10.0 should evaluate to 90.0"
    );
}
