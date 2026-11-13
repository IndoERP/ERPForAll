// SPDX-License-Identifier: MIT OR Apache-2.0
//! Legacy supplier unit migration, including transaction rollback on invalid links.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;

async fn legacy(tx: &mut Transaction) {
    // Deliberately tiny legacy tables exercise pre-schema SQL without a new registry.
    tx.batch("CREATE TEMP TABLE uom_uom(id bigint PRIMARY KEY);
        INSERT INTO uom_uom VALUES (20);
        CREATE TEMP TABLE product_template(id bigint PRIMARY KEY, uom_id bigint);
        CREATE TEMP TABLE product_product(id bigint PRIMARY KEY, product_tmpl_id bigint);
        CREATE TEMP TABLE product_supplierinfo(id bigint PRIMARY KEY, product_id bigint,
            product_tmpl_id bigint, company_id bigint, price numeric, currency_id bigint, min_qty numeric);
        INSERT INTO product_template VALUES (10, 20);
        INSERT INTO product_product VALUES (30, 10);
        INSERT INTO product_supplierinfo VALUES (40, 30, 10, NULL, 12.345, 50, NULL), (41, NULL, 10, 60, 9.99, 50, 2.5);")
        .await.unwrap();
}

#[tokio::test]
async fn migration_backfills_units_without_rewriting_agreements() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    legacy(&mut tx).await;
    erp_addon_product::migrations::migrate_supplier_units(
        &mut tx,
        &erp_sdk::migrations::SemverVersion::new(1, 0, 0),
    )
    .await
    .unwrap();
    let rows = tx.query("SELECT uom_id, company_id, price::text, currency_id FROM product_supplierinfo ORDER BY id", &[]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i64>(0), 20);
    assert_eq!(rows[0].get::<_, Option<i64>>(1), None);
    assert_eq!(rows[0].get::<_, String>(2), "12.345");
    assert_eq!(rows[1].get::<_, i64>(1), 60);
    let quantities = tx
        .query(
            "SELECT min_qty::text FROM product_supplierinfo ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        quantities[0].get::<_, Option<String>>(0),
        Some("0".to_owned())
    );
    assert_eq!(
        quantities[1].get::<_, Option<String>>(0),
        Some("2.5".to_owned())
    );
    erp_addon_product::migrations::migrate_supplier_units(
        &mut tx,
        &erp_sdk::migrations::SemverVersion::new(1, 0, 0),
    )
    .await
    .unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM product_supplierinfo WHERE uom_id = 20",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
}

#[tokio::test]
async fn migration_names_bad_rows_and_rolls_back_its_column() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    legacy(&mut tx).await;
    tx.execute(
        "UPDATE product_template SET uom_id = NULL WHERE id = 10",
        &[],
    )
    .await
    .unwrap();
    let error = tx
        .savepoint(async |tx| {
            erp_addon_product::migrations::migrate_supplier_units(
                tx,
                &erp_sdk::migrations::SemverVersion::new(1, 0, 0),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "Vendor unit migration refused 2 supplier rows with missing or inconsistent product/unit links; first IDs: 40, 41."
    );
    let rows = tx.query("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid = 'product_supplierinfo'::regclass AND attname = 'uom_id' AND NOT attisdropped)", &[]).await.unwrap();
    assert!(!rows[0].get::<_, bool>(0));
}
