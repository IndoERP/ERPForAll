// SPDX-License-Identifier: MIT OR Apache-2.0
//! Barcode backfill on actual holder tables, with rollback on invalid legacy data.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

async fn legacy_holder(tx: &mut Transaction, name: &str, company: Option<i64>) -> i64 {
    let rows = tx
        .env()
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("company_id", company.map_or(Cell::Null, Cell::Id)),
            ],
        )
        .await
        .unwrap();
    rows.rec_ids()[0].get()
}

#[tokio::test]
async fn backfill_collapses_mirrors_and_preserves_cross_company_reuse() {
    let h = support::base_harness().await;
    let a = support::seed_company(&h, "Upgrade A").await;
    let b = support::seed_company(&h, "Upgrade B").await;
    let mut tx = h.begin().await.unwrap();
    let first = legacy_holder(&mut tx, "Legacy A", Some(a)).await;
    let second = legacy_holder(&mut tx, "Legacy B", Some(b)).await;
    tx.execute(
        "UPDATE product_product SET barcode='LEGACY' WHERE product_tmpl_id=ANY($1)",
        &[&vec![first, second]],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE product_template SET barcode='LEGACY' WHERE id=ANY($1)",
        &[&vec![first, second]],
    )
    .await
    .unwrap();
    erp_addon_product::migrations::migrate_barcode_claims(&mut tx, &SemverVersion::new(1, 1, 0))
        .await
        .unwrap();
    let claims = tx
        .query(
            "SELECT company_id, template_id FROM product_barcode_claim ORDER BY company_id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].get::<_, i64>(0), a);
    assert_eq!(claims[1].get::<_, i64>(0), b);
    assert_eq!(claims[0].get::<_, Option<i64>>(1), None);
    erp_addon_product::migrations::migrate_barcode_claims(&mut tx, &SemverVersion::new(1, 1, 0))
        .await
        .unwrap();
    assert_eq!(
        tx.query("SELECT count(*) FROM product_barcode_claim", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
}

#[tokio::test]
async fn invalid_legacy_rows_are_named_and_backfill_rolls_back() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = legacy_holder(&mut tx, "Invalid A", None).await;
    let second = legacy_holder(&mut tx, "Invalid B", None).await;
    tx.execute(
        "UPDATE product_product SET barcode='INVALID' WHERE product_tmpl_id=ANY($1)",
        &[&vec![first, second]],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE product_template SET barcode='INVALID' WHERE id=ANY($1)",
        &[&vec![first, second]],
    )
    .await
    .unwrap();
    let ids: Vec<i64> = tx
        .query(
            "SELECT id FROM product_product WHERE product_tmpl_id=ANY($1)",
            &[&vec![first, second]],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    let error = tx
        .savepoint(async |tx| {
            erp_addon_product::migrations::migrate_barcode_claims(tx, &SemverVersion::new(1, 1, 0))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert!(error.message().contains("migration refused"), "{error}");
    assert!(error.message().contains("INVALID"), "{error}");
    for id in ids {
        assert!(
            error
                .message()
                .contains(&format!("product.product row {id}")),
            "{error}"
        );
    }
    assert_eq!(
        tx.query("SELECT count(*) FROM product_barcode_claim", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        tx.query("SELECT count(*) FROM product_barcode_scope", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
}

#[tokio::test]
async fn migration_bounds_identity_sample_and_reports_total_conflicts() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut templates = Vec::new();
    for index in 0..12 {
        templates.push(legacy_holder(&mut tx, &format!("Sample {index}"), None).await);
    }
    tx.execute(
        "UPDATE product_product SET barcode='SAMPLE' WHERE product_tmpl_id=ANY($1)",
        &[&templates],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE product_template SET barcode='SAMPLE' WHERE id=ANY($1)",
        &[&templates],
    )
    .await
    .unwrap();
    let ids: Vec<i64> = tx
        .query(
            "SELECT id FROM product_product WHERE product_tmpl_id=ANY($1) ORDER BY id",
            &[&templates],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    let error = tx
        .savepoint(async |tx| {
            erp_addon_product::migrations::migrate_barcode_claims(tx, &SemverVersion::new(1, 2, 0))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    let sample = ids[..10]
        .iter()
        .map(|id| format!("product.product row {id} (shared)"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        error.message(),
        format!(
            "Barcode claim migration refused 12 conflicting rows (first 10 identities):\n\n- \"SAMPLE\" is held by {sample}\n\nHolders you have no access to are not named."
        )
    );
}

#[tokio::test]
async fn backfill_gives_a_legacy_mirror_to_its_variant_when_an_archived_sibling_exists() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl = legacy_holder(&mut tx, "Legacy archived sibling", None).await;
    let active: i64 = tx
        .query(
            "SELECT id FROM product_product WHERE product_tmpl_id=$1",
            &[&tmpl],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.env()
        .records("product.product")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // The old writer mirrored the only active variant.
    tx.execute(
        "UPDATE product_product SET barcode='LEGACY-MIRROR' WHERE id=$1",
        &[&active],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE product_template SET barcode='LEGACY-MIRROR' WHERE id=$1",
        &[&tmpl],
    )
    .await
    .unwrap();
    for _ in 0..2 {
        erp_addon_product::migrations::migrate_barcode_claims(
            &mut tx,
            &SemverVersion::new(1, 1, 0),
        )
        .await
        .unwrap();
        let row = &tx
            .query(
                "SELECT (SELECT barcode FROM product_template WHERE id=$1),
                    (SELECT barcode FROM product_product WHERE id=$2),
                    (SELECT variant_id FROM product_barcode_claim WHERE barcode='LEGACY-MIRROR'),
                    (SELECT count(*) FROM product_barcode_claim WHERE barcode='LEGACY-MIRROR')",
                &[&tmpl, &active],
            )
            .await
            .unwrap()[0];
        assert_eq!(row.get::<_, Option<String>>(0), None);
        assert_eq!(row.get::<_, String>(1), "LEGACY-MIRROR");
        assert_eq!(row.get::<_, Option<i64>>(2), Some(active));
        assert_eq!(row.get::<_, i64>(3), 1);
    }
}
