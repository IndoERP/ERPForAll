//! Product Combo tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = support::base_harness().await;

    let mut tx = h.begin().await.unwrap();
    tx.commit().await.unwrap();

    h
}

#[tokio::test]
async fn test_combo_archiving_protection() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();
    let combo_model = tx.env().records("product.combo").unwrap();
    let item_model = tx.env().records("product.combo.item").unwrap();

    // 1. Create a food product (e.g. Fries)
    let fries_tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Fries".into()))],
        )
        .await
        .unwrap();
    let fries_tmpl_id = fries_tmpl.rec_ids()[0].get();

    let d_fries = Domain::condition("product_tmpl_id", Operator::Eq, fries_tmpl_id).unwrap();
    let fries_var = prod_model.search(&mut tx, &d_fries).await.unwrap();
    let fries_var_id = fries_var.rec_ids()[0].get();

    // 2. Create combo "Sides" and link Fries
    let combo = combo_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Sides".into()))],
        )
        .await
        .unwrap();
    let combo_id = combo.rec_ids()[0].get();

    let combo_item = item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("combo_id", Cell::Id(combo_id))
                    .set_cell("product_id", Cell::Id(fries_var_id))
                    .set_cell("extra_price", Cell::Decimal(Decimal::ZERO)),
            ],
        )
        .await
        .unwrap();
    let combo_item_id = combo_item.rec_ids()[0].get();

    // Validation poisons the ORM transaction. Native callers that recover must wrap the
    // complete Records::write call, including its poison boundary, in a savepoint.
    let message = "This product belongs to a combo, so it cannot be archived. Remove it from its combos first.";
    let err_var = tx
        .savepoint(async |tx| {
            fries_var
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err_var.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(err_var.message(), message);

    let err_tmpl = tx
        .savepoint(async |tx| {
            fries_tmpl
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err_tmpl.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(err_tmpl.message(), message);

    // 5. Remove from combo -> archiving should succeed
    let item_rec = item_model.browse(vec![combo_item_id]).unwrap();
    item_rec.unlink(&mut tx).await.unwrap();
    let ok_var = fries_var
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await;
    assert!(
        ok_var.is_ok(),
        "Once removed from combo, product variant can be archived: {ok_var:?}"
    );
}

/// Test combo creation and extra pricing
#[tokio::test]
async fn test_combo_creation_and_extra_price() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();
    let combo_model = tx.env().records("product.combo").unwrap();
    let item_model = tx.env().records("product.combo.item").unwrap();

    let burger_tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Cheeseburger".into()))],
        )
        .await
        .unwrap();
    let burger_id = burger_tmpl.rec_ids()[0].get();
    let d = Domain::condition("product_tmpl_id", Operator::Eq, burger_id).unwrap();
    let burger_var_id = prod_model.search(&mut tx, &d).await.unwrap().rec_ids()[0].get();

    let combo = combo_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Main Choices".into()))],
        )
        .await
        .unwrap();
    let combo_id = combo.rec_ids()[0].get();

    let item = item_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("combo_id", Cell::Id(combo_id))
                    .set_cell("product_id", Cell::Id(burger_var_id))
                    .set_cell("extra_price", Cell::Decimal(Decimal::new(250, 2))),
            ],
        )
        .await
        .unwrap();

    let extra = item.cell(&mut tx, "extra_price").await.unwrap();
    assert_eq!(extra, Cell::Decimal(Decimal::new(250, 2)));
}
