//! Hook N6: the overridable `_get_standard_price` method and the paths that read cost through it.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_addon_product::{ProductPricelist, ProductProduct, ProductProductMethods};
use erp_sdk::prelude::*;

async fn variant_with_cost(tx: &mut Transaction, cost: Decimal) -> (i64, i64) {
    let env = support::company_env(tx).await;
    let tmpl = env
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Hooked".into()))
                    .set_cell("standard_price", Cell::Decimal(cost)),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();
    let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant = env
        .records("product.product")
        .unwrap()
        .search(tx, &domain)
        .await
        .unwrap();
    (tmpl_id, variant.rec_ids()[0].get())
}

/// stock receipt itself belongs to M7)
#[tokio::test]
async fn a_later_addon_supplies_the_cost_without_writing_the_row() {
    let h = support::base_harness().await;
    support::seed_company(&h, "Hook Company").await;
    let mut tx = h.begin().await.unwrap();
    let stored = Decimal::new(1000, 2);
    let (tmpl_id, variant_id) = variant_with_cost(&mut tx, stored).await;
    let env = support::company_env(&mut tx).await;
    let plain = env
        .records("product.product")
        .unwrap()
        .browse(vec![variant_id])
        .unwrap();
    let plain_set = RecordSet::<ProductProduct>::try_from(plain.clone()).unwrap();

    // without a provider the stored company-dependent cost is returned
    assert_eq!(
        plain_set._get_standard_price(&mut tx).await.unwrap(),
        vec![stored]
    );

    // with the provider the override answers, through `sup` for everything else
    let probed = plain.with_context([("test_cost_probe", Value::Int(4250))]);
    let probed_set = RecordSet::<ProductProduct>::try_from(probed).unwrap();
    assert_eq!(
        probed_set._get_standard_price(&mut tx).await.unwrap(),
        vec![Decimal::new(4250, 2)]
    );

    // the product row was never written by the provider
    assert_eq!(
        plain.cell(&mut tx, "standard_price").await.unwrap(),
        Cell::Decimal(stored)
    );

    // the template cost reads through the same method
    let tmpl = env
        .records("product.template")
        .unwrap()
        .with_context([("test_cost_probe", Value::Int(4250))])
        .browse(vec![tmpl_id])
        .unwrap();
    assert_eq!(
        tmpl.cell(&mut tx, "standard_price").await.unwrap(),
        Cell::Decimal(Decimal::new(4250, 2))
    );
}

/// The pricelist cost base reads through the hook and never falls back to the template cost
#[tokio::test]
async fn the_pricelist_cost_base_uses_the_hook() {
    let h = support::base_harness().await;
    support::seed_company(&h, "Hook Company").await;
    let mut tx = h.begin().await.unwrap();
    let (_tmpl_id, variant_id) = variant_with_cost(&mut tx, Decimal::ZERO).await;
    let env = support::company_env(&mut tx).await;
    let pl = env
        .records("product.pricelist")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Cost PL".into()))],
        )
        .await
        .unwrap();
    env.records("product.pricelist.item")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("pricelist_id", Cell::Id(pl.rec_ids()[0].get()))
                    .set_cell("base", Cell::Str("standard_price".into()))
                    .set_cell("compute_price", Cell::Str("discount".into()))
                    .set_cell("price_discount", Cell::Decimal(Decimal::ZERO)),
            ],
        )
        .await
        .unwrap();

    // a stored cost of 0 stays 0: it is not replaced by another value
    let plain = RecordSet::<ProductPricelist>::try_from(pl.clone()).unwrap();
    let price =
        ProductPricelist::get_product_price(&plain, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(price, Decimal::ZERO);

    // with the provider the rule is priced from the supplied cost
    let probed = RecordSet::<ProductPricelist>::try_from(
        pl.with_context([("test_cost_probe", Value::Int(4250))]),
    )
    .unwrap();
    let price =
        ProductPricelist::get_product_price(&probed, &mut tx, variant_id, Decimal::ONE, None)
            .await
            .unwrap();
    assert_eq!(price, Decimal::new(4250, 2));
}
