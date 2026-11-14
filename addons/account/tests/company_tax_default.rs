// SPDX-License-Identifier: MIT OR Apache-2.0
//! The company tax default added by `account`: required, default `tax_excluded`, filled by every ORM create.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::prelude::*;
use erp_testkit::Harness;

/// company is created through the ORM without it; the override resolution is tested with the tax model)
#[tokio::test]
async fn a_company_created_without_the_price_inclusion_gets_tax_excluded() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let created = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("No inclusion".into()))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let stored: String = tx
        .query(
            "SELECT account_price_include FROM res_company WHERE id = $1",
            &[&created.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, "tax_excluded");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
