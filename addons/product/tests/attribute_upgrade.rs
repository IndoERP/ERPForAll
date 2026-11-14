// SPDX-License-Identifier: MIT OR Apache-2.0
//! Legacy activity defaults do not rewrite configured identities or sequences.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;

#[tokio::test]
async fn legacy_attributes_gain_activity_without_rewriting_catalog_values() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "CREATE TEMP TABLE product_attribute(id bigint, sequence integer);
        CREATE TEMP TABLE product_attribute_value(id bigint, default_extra_price numeric);
        INSERT INTO product_attribute VALUES (10,1),(11,42);
        INSERT INTO product_attribute_value VALUES (20,12.345),(21,0);",
    )
    .await
    .unwrap();
    for _ in 0..2 {
        erp_addon_product::migrations::migrate_attribute_activity(
            &mut tx,
            &erp_sdk::migrations::SemverVersion::new(1, 1, 0),
        )
        .await
        .unwrap();
    }
    let rows = tx
        .query(
            "SELECT id, sequence, active FROM product_attribute ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.get::<_, i64>(0), r.get::<_, i32>(1), r.get::<_, bool>(2)))
            .collect::<Vec<_>>(),
        [(10, 1, true), (11, 42, true)]
    );
    let rows = tx
        .query(
            "SELECT id, default_extra_price::text, active FROM product_attribute_value ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.get::<_, i64>(0),
                r.get::<_, String>(1),
                r.get::<_, bool>(2)
            ))
            .collect::<Vec<_>>(),
        [(20, "12.345".to_owned(), true), (21, "0".to_owned(), true)]
    );
}

#[tokio::test]
async fn activity_migration_preserves_archived_records() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "CREATE TEMP TABLE product_attribute(id bigint, active boolean);
        CREATE TEMP TABLE product_attribute_value(id bigint, active boolean);
        INSERT INTO product_attribute VALUES (10,false),(11,NULL);
        INSERT INTO product_attribute_value VALUES (20,false),(21,NULL);",
    )
    .await
    .unwrap();
    erp_addon_product::migrations::migrate_attribute_activity(
        &mut tx,
        &erp_sdk::migrations::SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap();
    for sql in [
        "SELECT active FROM product_attribute ORDER BY id",
        "SELECT active FROM product_attribute_value ORDER BY id",
    ] {
        let rows = tx.query(sql, &[]).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.get::<_, bool>(0)).collect::<Vec<_>>(),
            [false, true]
        );
    }
}
