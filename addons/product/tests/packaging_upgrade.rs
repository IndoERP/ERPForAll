// SPDX-License-Identifier: MIT OR Apache-2.0
//! Historical catalog reference backfill, repeat upgrade and rollback.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::prelude::*;

#[tokio::test]
async fn historical_packaging_references_are_backfilled_idempotently() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let u = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Historical pack".into()))
                    .set_cell("relative_uom_id", Cell::Id(root))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(6))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    // Raw legacy data deliberately bypasses the new catalog hooks.
    tx.execute("INSERT INTO product_template(name, uom_id, active, type) VALUES ('{\"en_US\":\"Legacy\"}'::jsonb, $1, false, 'consu')", &[&u]).await.unwrap();
    let version = erp_sdk::migrations::SemverVersion::new(1, 1, 0);
    erp_addon_product::migrations::migrate_packaging_references(&mut tx, &version)
        .await
        .unwrap();
    erp_addon_product::migrations::migrate_packaging_references(&mut tx, &version)
        .await
        .unwrap();
    let rows = tx
        .query(
            "SELECT root_id, factor::text FROM uom_reference_use WHERE uom_id = $1",
            &[&u],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i64>(0), root);
    assert_eq!(
        rows[0].get::<_, String>(1).parse::<Decimal>().unwrap(),
        Decimal::from(6)
    );
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("uom.uom")
                .unwrap()
                .browse([u])
                .unwrap()
                .write(
                    tx,
                    RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(7))),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A referenced unit of measure would change. Create a new unit instead."
    );
}
