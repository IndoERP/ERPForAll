//! Supplierinfo tests including BUG-088 invariant.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = support::base_harness().await;
    support::seed_company(&h, "Supplier Company").await;
    h
}

#[tokio::test]
async fn test_supplierinfo_date_validity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partner_model = support::company_env(&mut tx)
        .await
        .records("res.partner")
        .unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let seller_model = support::company_env(&mut tx)
        .await
        .records("product.supplierinfo")
        .unwrap();

    let partner = partner_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Test Supplier".into()))],
        )
        .await
        .unwrap();
    let partner_id = partner.rec_ids()[0].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Supplier Test Product".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let currency_id = match tmpl.cell(&mut tx, "currency_id").await.unwrap() {
        Cell::Id(id) => id,
        _ => 1,
    };

    // Start date > End date should fail
    let err = seller_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(partner_id))
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("currency_id", Cell::Id(currency_id))
                    .set_cell(
                        "date_start",
                        Cell::Date(Date::from_ymd(2026, 1, 15).unwrap()),
                    )
                    .set_cell("date_end", Cell::Date(Date::from_ymd(2026, 1, 10).unwrap())),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Start date greater than end date must be refused"
    );
}

#[tokio::test]
async fn test_supplierinfo_variant_template_mismatch_refused_bug_088() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partner_model = support::company_env(&mut tx)
        .await
        .records("res.partner")
        .unwrap();
    let tmpl_model = support::company_env(&mut tx)
        .await
        .records("product.template")
        .unwrap();
    let prod_model = support::company_env(&mut tx)
        .await
        .records("product.product")
        .unwrap();
    let seller_model = support::company_env(&mut tx)
        .await
        .records("product.supplierinfo")
        .unwrap();

    let partner = partner_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Vendor A".into()))],
        )
        .await
        .unwrap();
    let partner_id = partner.rec_ids()[0].get();

    // Template 1
    let tmpl1 = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Product 1".into()))],
        )
        .await
        .unwrap();
    let tmpl1_id = tmpl1.rec_ids()[0].get();

    // Template 2
    let tmpl2 = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Product 2".into()))],
        )
        .await
        .unwrap();
    let tmpl2_id = tmpl2.rec_ids()[0].get();

    let currency_id = match tmpl1.cell(&mut tx, "currency_id").await.unwrap() {
        Cell::Id(id) => id,
        _ => 1,
    };

    // Find variant of template 2
    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl2_id).unwrap();
    let variants2 = prod_model.search(&mut tx, &d).await.unwrap();
    let var2_id = variants2.rec_ids()[0].get();

    // Attempt to create supplierinfo on template 1 with variant belonging to template 2
    let err = seller_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(partner_id))
                    .set_cell("product_tmpl_id", Cell::Id(tmpl1_id))
                    .set_cell("product_id", Cell::Id(var2_id))
                    .set_cell("currency_id", Cell::Id(currency_id)),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "BUG-088: Mismatched variant and template must be refused"
    );
}
