//! Packaging barcodes (`product.uom`): and the product-side check.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;

async fn product(tx: &mut Transaction, name: &str, barcode: Option<&str>) -> i64 {
    let tmpl_model = tx.env().records("product.template").unwrap();
    let mut vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(b) = barcode {
        vals = vals.set_cell("barcode", Cell::Str(b.into()));
    }
    let tmpl = tmpl_model.create(tx, vec![vals]).await.unwrap();
    let domain =
        Domain::condition("product_tmpl_id", Operator::Eq, tmpl.rec_ids()[0].get()).unwrap();
    tx.env()
        .records("product.product")
        .unwrap()
        .search(tx, &domain)
        .await
        .unwrap()
        .rec_ids()[0]
        .get()
}

async fn packaging(
    tx: &mut Transaction,
    product_id: i64,
    unit: i64,
    barcode: &str,
) -> Result<Records> {
    tx.env()
        .records("product.uom")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("product_id", Cell::Id(product_id))
                    .set_cell("uom_id", Cell::Id(unit))
                    .set_cell("barcode", Cell::Str(barcode.into())),
            ],
        )
        .await
}

#[tokio::test]
async fn a_packaging_barcode_is_unique() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let p = product(&mut tx, "Water", None).await;
    packaging(&mut tx, p, unit, "01012345678901").await.unwrap();
    let mut inner = tx;
    let err = inner
        .savepoint(async |tx| packaging(tx, p, unit, "01012345678901").await)
        .await
        .expect_err("the same packaging barcode twice");
    assert!(
        err.message()
            .contains("A barcode can only be assigned to one packaging.")
            || err.message().contains("_barcode_uniq"),
        "unexpected error: {}",
        err.message()
    );
}

#[tokio::test]
async fn a_packaging_barcode_cannot_equal_a_product_barcode() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let with_barcode = product(&mut tx, "Soda", Some("11223344")).await;
    let err = packaging(&mut tx, with_barcode, unit, "11223344")
        .await
        .expect_err("a product already uses this barcode");
    assert_eq!(err.message(), "A product already uses the barcode");
}

#[tokio::test]
async fn a_product_barcode_cannot_equal_a_packaging_barcode() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let p = product(&mut tx, "Juice", None).await;
    packaging(&mut tx, p, unit, "PKG-555").await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let err = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Other".into()))
                    .set_cell("barcode", Cell::Str("PKG-555".into())),
            ],
        )
        .await
        .expect_err("a packaging already uses this barcode");
    assert_eq!(err.message(), "A packaging already uses the barcode");
}

#[tokio::test]
async fn a_deleted_packaging_frees_its_barcode() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let p = product(&mut tx, "Cola", None).await;
    let pack = packaging(&mut tx, p, unit, "PKG-777").await.unwrap();
    pack.unlink(&mut tx).await.unwrap();
    product(&mut tx, "Cola 2", Some("PKG-777")).await;
}

/// The packaging relations on the unit and on the product list the rows
#[tokio::test]
async fn unit_and_product_list_their_packagings() {
    let h = support::base_harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let p = product(&mut tx, "Beer", None).await;
    let pack = packaging(&mut tx, p, unit, "PKG-1").await.unwrap();
    let pack_id = pack.rec_ids()[0];
    let unit_rec = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse(vec![unit])
        .unwrap();
    let prod_rec = tx
        .env()
        .records("product.product")
        .unwrap()
        .browse(vec![p])
        .unwrap();
    assert_eq!(
        unit_rec.cell(&mut tx, "product_uom_ids").await.unwrap(),
        Cell::Ids(vec![pack_id].into())
    );
    assert_eq!(
        prod_rec.cell(&mut tx, "product_uom_ids").await.unwrap(),
        Cell::Ids(vec![pack_id].into())
    );
}
