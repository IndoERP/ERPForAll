//! Upgrading a database whose rates predate rate types (step 1).
//!
//! The old shape is made by taking the new schema back: the type table and the column are dropped,
//! then the 1.6 pre-migration runs and the ordinary schema step follows, as the module lifecycle
//! orders them. This is a simulation of the old schema, not a genuine old-release upgrade: it does
//! not restore the old three-column key, so the key-replacement path of the schema step is not
//! exercised here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/rate_type_fixture.rs"]
mod rate_type_fixture;

use erp_addon_base::migrations::prepare_rate_types;
use erp_sdk::money::parse_decimal;
use erp_sdk::{Cell, Date, RawVals};
use erp_test_models::base_users;
use erp_testkit::Harness;

#[tokio::test]
async fn existing_rates_become_spot_before_the_key_switches() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    rate_type_fixture::seed_rate_types(&h).await;
    let mut tx = h.begin().await.unwrap();
    let text = |s: &str| Cell::Str(s.into());
    let currencies = tx.env().records("res.currency").unwrap();
    let mut ids = Vec::new();
    for code in ["USD", "EUR"] {
        let vals = RawVals::new()
            .set_cell("name", text(code))
            .set_cell("symbol", text(code))
            .set_cell("rounding", Cell::Decimal(parse_decimal("0.01").unwrap()))
            .set_cell("active", Cell::Bool(true));
        ids.push(
            currencies
                .create(&mut tx, vec![vals])
                .await
                .unwrap()
                .rec_ids()[0],
        );
    }
    let rates = tx.env().records("res.currency.rate").unwrap();
    for (day, rate) in [("2026-01-01", "1.1"), ("2026-02-01", "1.2")] {
        let vals = RawVals::new()
            .set_cell("currency_id", Cell::Id(ids[1].get()))
            .set_cell("name", Cell::Date(Date::parse_odoo(day).unwrap()))
            .set_cell("rate", Cell::Decimal(parse_decimal(rate).unwrap()));
        rates.create(&mut tx, vec![vals]).await.unwrap();
    }
    tx.flush().await.unwrap();

    // the old shape: no type table, no column
    tx.batch("ALTER TABLE res_currency_rate DROP COLUMN rate_type_id")
        .await
        .unwrap();
    tx.batch("DROP TABLE res_currency_rate_type CASCADE")
        .await
        .unwrap();
    tx.batch("DELETE FROM ir_model_data WHERE name = 'rate_type_spot'")
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();

    prepare_rate_types(&mut tx).await.unwrap();
    // the schema step the lifecycle runs next must accept the now-required column
    tx.init_models(&[]).await.unwrap();

    let rows = tx
        .query(
            "SELECT t.code, count(*) FROM res_currency_rate r \
             JOIN res_currency_rate_type t ON t.id = r.rate_type_id GROUP BY t.code",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [("spot".to_owned(), 2)]);
    let kept = tx
        .query(
            "SELECT to_char(name, 'YYYY-MM-DD'), rate::text, company_id IS NULL FROM res_currency_rate ORDER BY name",
            &[],
        )
        .await
        .unwrap();
    let kept: Vec<(String, String, bool)> = kept
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        kept,
        [
            ("2026-01-01".to_owned(), "1.1".to_owned(), true),
            ("2026-02-01".to_owned(), "1.2".to_owned(), true)
        ],
        "values, dates and companies are unchanged"
    );
    let spots = tx
        .query(
            "SELECT count(*) FROM res_currency_rate_type WHERE code = 'spot'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(spots[0].get::<_, i64>(0), 1, "exactly one spot type");
    let xid = tx
        .query(
            "SELECT noupdate FROM ir_model_data d JOIN res_currency_rate_type t ON t.id = d.res_id \
             WHERE d.module = 'base' AND d.name = 'rate_type_spot' AND d.model = 'res.currency.rate.type' \
             AND t.code = 'spot'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(xid.len(), 1);
    assert!(xid[0].get::<_, bool>(0));
    // running it again changes nothing
    prepare_rate_types(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
