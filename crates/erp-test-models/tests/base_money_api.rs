//! S8 of the money review: the public shape of the result types. Kept in its own test binary so a
//! compile error on the old shape cannot hide the behavioural tests of `base_money_review.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/rate_type_fixture.rs"]
mod rate_type_fixture;

use erp_addon_base::money::{self, Conversion};
use erp_sdk::money::{RateDateRule, RoundingMode, parse_decimal};
use erp_sdk::{Cell, Date, RawVals};
use erp_test_models::base_users;
use erp_testkit::Harness;

/// A `Conversion` is read through getters: its fields are private, so a later field cannot break
/// callers and no caller can build one by hand.
fn read_through_getters(conversion: &Conversion) -> String {
    format!(
        "{} {} {}",
        conversion.amount(),
        conversion.exact(),
        conversion.snapshot().to_currency()
    )
}

#[tokio::test]
async fn s8_conversion_is_read_through_getters() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    rate_type_fixture::seed_rate_types(&h).await;
    let mut tx = h.begin().await.unwrap();
    let mut ids = Vec::new();
    for (code, rate) in [("USD", None), ("EUR", Some("0.85"))] {
        let vals = RawVals::new()
            .set_cell("name", Cell::Str(code.into()))
            .set_cell("symbol", Cell::Str(code.into()))
            .set_cell("rounding", Cell::Decimal(parse_decimal("0.01").unwrap()))
            .set_cell("active", Cell::Bool(true));
        let records = tx.env().records("res.currency").unwrap();
        let id = records.create(&mut tx, vec![vals]).await.unwrap().rec_ids()[0];
        ids.push((id, rate));
    }
    let (usd, eur) = (ids[0].0, ids[1].0);
    let companies = tx.env().records("res.company").unwrap();
    let company = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Root".into()))
                    .set_cell("currency_id", Cell::Id(usd.get())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let day = Date::from_ymd(2026, 6, 1).unwrap();
    let rates = tx.env().records("res.currency.rate").unwrap();
    rates
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(eur.get()))
                    .set_cell("name", Cell::Date(Date::from_ymd(2026, 1, 1).unwrap()))
                    .set_cell("rate", Cell::Decimal(parse_decimal("0.85").unwrap()))
                    .set_cell("company_id", Cell::Id(company.get())),
            ],
        )
        .await
        .unwrap();
    let env = tx.env();
    let conversion = money::convert_with(
        &env,
        &mut tx,
        parse_decimal("100").unwrap(),
        usd,
        eur,
        company,
        day,
        RateDateRule::StrictlyBefore,
        RoundingMode::HalfAwayFromZero,
    )
    .await
    .unwrap();
    assert_eq!(read_through_getters(&conversion), "85.00 85 EUR");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
