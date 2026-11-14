//! Money and currency services (H41) and dated parameters (H35) of the base addon.
//! The exact primitives are tested in `crates/erp-fields/tests/money.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;
#[path = "support/rate_type_fixture.rs"]
mod rate_type_fixture;

use std::cmp::Ordering;
use std::num::NonZeroU64;

use erp_addon_base::dated_parameter::{self, ParamKind, ParamValue};
use erp_addon_base::decimal_precision::{
    self, DEFAULT_PRECISION_DIGITS, FieldScale, precision_defined, precision_get,
};
use erp_addon_base::money::{self, CurrencyRounding, RateBook, RoundingTable};
use erp_orm::{
    BeginOptions, CompanyId, Context, GroupSet, Operation, PrincipalBuilder, Resolution, UserId,
};
use erp_sdk::money::{RateDateRule, RateOrigin, RoundingMode, canonical_string, parse_decimal};
use erp_sdk::{Cell, Date, Decimal, Domain, Env, ErrorKind, RawVals, RecId, Transaction, Value};
use erp_test_models::{base_users, test_defaults, test_orm};
use erp_testkit::{DomainPolicy, Harness, Otherwise};

async fn harness() -> Harness {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
        &test_orm::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .build()
    .await
    .expect("harness");
    crate::rate_type_fixture::seed_rate_types(&h).await;
    h
}

fn dec(text: &str) -> Decimal {
    parse_decimal(text).unwrap()
}

fn day(text: &str) -> Date {
    Date::parse_odoo(text).unwrap()
}

fn text_cell(text: &str) -> Cell {
    Cell::Str(text.into())
}

async fn currency(tx: &mut Transaction, code: &str, rounding: &str) -> RecId {
    let vals = RawVals::new()
        .set_cell("name", text_cell(code))
        .set_cell("symbol", text_cell(code))
        .set_cell("rounding", Cell::Decimal(dec(rounding)))
        .set_cell("active", Cell::Bool(true));
    let records = tx.env().records("res.currency").unwrap();
    records.create(tx, vec![vals]).await.unwrap().rec_ids()[0]
}

async fn company(
    tx: &mut Transaction,
    name: &str,
    currency: RecId,
    parent: Option<RecId>,
) -> RecId {
    let mut vals = RawVals::new()
        .set_cell("name", text_cell(name))
        .set_cell("currency_id", Cell::Id(currency.get()));
    if let Some(parent) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(parent.get()));
    }
    let records = tx.env().records("res.company").unwrap();
    records.create(tx, vec![vals]).await.unwrap().rec_ids()[0]
}

fn rate_vals(currency: RecId, company: Option<RecId>, on: &str, rate: &str) -> RawVals {
    let mut vals = RawVals::new()
        .set_cell("currency_id", Cell::Id(currency.get()))
        .set_cell("name", Cell::Date(day(on)))
        .set_cell("rate", Cell::Decimal(dec(rate)));
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company.get()));
    }
    vals
}

async fn rate(
    tx: &mut Transaction,
    currency: RecId,
    company: Option<RecId>,
    on: &str,
    rate: &str,
) -> RecId {
    let records = tx.env().records("res.currency.rate").unwrap();
    records
        .create(tx, vec![rate_vals(currency, company, on, rate)])
        .await
        .unwrap()
        .rec_ids()[0]
}

async fn book(
    tx: &mut Transaction,
    company: RecId,
    on: &str,
    rule: RateDateRule,
    currencies: &[RecId],
) -> erp_sdk::Result<RateBook> {
    let env = tx.env();
    RateBook::load(&env, tx, company, day(on), rule, currencies).await
}

async fn rounding_of(tx: &mut Transaction, currency: RecId) -> CurrencyRounding {
    let env = tx.env();
    RoundingTable::load(&env, tx, &[currency])
        .await
        .unwrap()
        .get(currency)
        .unwrap()
        .clone()
}

// ---------------------------------------------------------------------------------------------
// rate lookup by date, per company
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn spec_base_061_historical_rate_selection_by_date() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let (first, second) = (dec("1.10"), dec("1.15"));
    rate(&mut tx, eur, Some(root), "2026-01-01", "1.10").await;
    rate(&mut tx, eur, Some(root), "2026-06-01", "1.15").await;

    let strict = book(
        &mut tx,
        root,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    let snapshot = strict.snapshot(usd, eur).unwrap();
    let (_, eur_rate) = snapshot.rates().unwrap();
    assert_eq!(eur_rate.rate(), first);
    assert_eq!(eur_rate.rate_date(), Some(day("2026-01-01")));
    assert_eq!(eur_rate.origin(), RateOrigin::Preceding);

    // the explicit on-or-before rule takes the rate stated for that day
    let inclusive = book(
        &mut tx,
        root,
        "2026-06-01",
        RateDateRule::OnOrBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    let snapshot = inclusive.snapshot(usd, eur).unwrap();
    let (_, eur_rate) = snapshot.rates().unwrap();
    assert_eq!(eur_rate.rate(), second);
    assert_eq!(eur_rate.rate_date(), Some(day("2026-06-01")));

    // a later date sees the newest rate under both rules
    for rule in [RateDateRule::StrictlyBefore, RateDateRule::OnOrBefore] {
        let later = book(&mut tx, root, "2026-07-01", rule, &[usd, eur])
            .await
            .unwrap();
        assert_eq!(
            later.snapshot(usd, eur).unwrap().rates().unwrap().1.rate(),
            second
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the spec case does not exercise)
#[tokio::test]
async fn rate_before_the_first_rate_falls_back_to_the_earliest_later_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    rate(&mut tx, eur, Some(root), "2026-03-01", "1.30").await;
    rate(&mut tx, eur, Some(root), "2026-09-01", "1.40").await;
    let early = book(
        &mut tx,
        root,
        "2026-01-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    let snapshot = early.snapshot(usd, eur).unwrap();
    let (_, eur_rate) = snapshot.rates().unwrap();
    assert_eq!(eur_rate.rate(), dec("1.30"));
    assert_eq!(eur_rate.origin(), RateOrigin::LaterFallback);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_062_rates_resolve_against_the_root_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let branch = company(&mut tx, "BranchA", usd, Some(root)).await;
    let outsider = company(&mut tx, "Outsider", usd, None).await;
    rate(&mut tx, eur, Some(root), "2026-01-01", "1.10").await;

    let from_branch = book(
        &mut tx,
        branch,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(from_branch.root_company_id(), root.get());
    assert_eq!(
        from_branch
            .snapshot(usd, eur)
            .unwrap()
            .rates()
            .unwrap()
            .1
            .rate(),
        dec("1.10")
    );

    // a company of another hierarchy has no rate of the root company: loud, never 1.0
    let from_outsider = book(
        &mut tx,
        outsider,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(from_outsider.root_company_id(), outsider.get());
    let error = from_outsider.snapshot(usd, eur).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains("EUR"), "{}", error.message());

    // a rate shared by all companies serves the outsider; the root's own rate still wins
    rate(&mut tx, eur, None, "2026-03-01", "1.30").await;
    let shared = book(
        &mut tx,
        outsider,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(
        shared.snapshot(usd, eur).unwrap().rates().unwrap().1.rate(),
        dec("1.30")
    );
    let own = book(
        &mut tx,
        root,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(
        own.snapshot(usd, eur).unwrap().rates().unwrap().1.rate(),
        dec("1.10")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cyclic_company_hierarchy_is_an_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let a = company(&mut tx, "A", usd, None).await;
    let b = company(&mut tx, "B", usd, Some(a)).await;
    tx.execute(
        "UPDATE res_company SET parent_id = $2 WHERE id = $1",
        &[&a.get(), &b.get()],
    )
    .await
    .unwrap();
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let error = book(
        &mut tx,
        a,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(error.message().contains("cyclic"), "{}", error.message());
    let ghost = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([999_999])
        .unwrap()
        .rec_ids()[0];
    let missing = book(
        &mut tx,
        ghost,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd],
    )
    .await
    .unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::Missing);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_rate_is_an_error_never_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let chf = currency(&mut tx, "CHF", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let env = tx.env();
    for amount in ["100", "0"] {
        let error = money::convert(
            &env,
            &mut tx,
            dec(amount),
            usd,
            chf,
            root,
            day("2026-06-01"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert!(error.message().contains("CHF"), "{}", error.message());
        assert!(
            error.message().contains("2026-06-01"),
            "{}",
            error.message()
        );
    }
    // the company currency is the reference: it needs no row, and the identity needs none either
    let same = money::convert(
        &env,
        &mut tx,
        dec("12.345"),
        usd,
        usd,
        root,
        day("2026-06-01"),
    )
    .await
    .unwrap();
    assert_eq!(same, dec("12.35"));
    // a currency that does not exist is not a missing rate
    let ghost = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([999_999])
        .unwrap()
        .rec_ids()[0];
    let error = money::convert(&env, &mut tx, dec("1"), usd, ghost, root, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Missing);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// conversion and rounding
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn spec_base_070_currency_conversion_calculation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let eur_per_usd = "0.85";
    rate(&mut tx, eur, Some(root), "2026-01-01", eur_per_usd).await;
    let env = tx.env();

    let converted = money::convert(&env, &mut tx, dec("100"), usd, eur, root, day("2026-06-01"))
        .await
        .unwrap();
    // 100 x 0.85, computed by the decimal library independently of the exact ratio path
    assert_eq!(converted, dec("100") * dec(eur_per_usd));
    assert_eq!(converted.scale(), 2, "rounded to EUR's increment");

    let back = money::convert(&env, &mut tx, converted, eur, usd, root, day("2026-06-01"))
        .await
        .unwrap();
    assert_eq!(back, dec("100"));

    // the evidence of a conversion names both rates and the direction
    let conversion = money::convert_with(
        &env,
        &mut tx,
        dec("100"),
        usd,
        eur,
        root,
        day("2026-06-01"),
        RateDateRule::StrictlyBefore,
        RoundingMode::HalfAwayFromZero,
    )
    .await
    .unwrap();
    assert_eq!(conversion.snapshot().from_currency().to_string(), "USD");
    assert_eq!(conversion.snapshot().to_currency().to_string(), "EUR");
    let (from_rate, to_rate) = conversion.snapshot().rates().unwrap();
    assert_eq!(from_rate.origin(), RateOrigin::ReferenceCurrency);
    assert_eq!(from_rate.rate(), Decimal::ONE);
    assert!(to_rate.rate_id().is_some());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_085_zero_decimal_currency_has_no_fractional_drift() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let idr = currency(&mut tx, "IDR", "1.0").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    rate(&mut tx, idr, Some(root), "2026-01-01", "15999.5").await;
    let env = tx.env();

    let idr_rounding = rounding_of(&mut tx, idr).await;
    assert_eq!(idr_rounding.decimal_places(), 0);

    for amount in ["10.01", "0.5", "123456.789", "-3.33"] {
        let converted = money::convert(
            &env,
            &mut tx,
            dec(amount),
            usd,
            idr,
            root,
            day("2026-06-01"),
        )
        .await
        .unwrap();
        let expected = (dec(amount) * dec("15999.5"))
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
        assert_eq!(converted, expected, "{amount}");
        assert_eq!(
            converted.scale(),
            0,
            "{amount} must have no fractional digits"
        );
        assert_eq!(converted.fract(), Decimal::ZERO);
    }
    // a rate with no finite decimal factor still rounds only once, at the end
    let three = currency(&mut tx, "TRI", "0.01").await;
    rate(&mut tx, three, Some(root), "2026-01-01", "3").await;
    let third = money::convert(&env, &mut tx, dec("1"), three, usd, root, day("2026-06-01"))
        .await
        .unwrap();
    assert_eq!(third, dec("0.33"));
    let two_thirds = money::convert(&env, &mut tx, dec("2"), three, usd, root, day("2026-06-01"))
        .await
        .unwrap();
    assert_eq!(
        two_thirds,
        dec("0.67"),
        "2/3 rounded once, not twice the rounded 1/3"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_063_half_up_tie_breaks_exactly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let rounding = rounding_of(&mut tx, usd).await;
    let tie = dec("2.675");
    assert_eq!(
        rounding.round(tie).unwrap(),
        tie.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
    );
    assert_eq!(rounding.round(tie).unwrap(), dec("2.68"));
    assert_eq!(rounding.round(-tie).unwrap(), dec("-2.68"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_064_half_even_bankers_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let whole = currency(&mut tx, "WHL", "1").await;
    let rounding = rounding_of(&mut tx, whole).await;
    for (value, expected) in [("2.5", "2"), ("3.5", "4")] {
        assert_eq!(
            rounding
                .round_with(dec(value), RoundingMode::HalfEven)
                .unwrap(),
            dec(value)
                .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointNearestEven)
        );
        assert_eq!(
            rounding
                .round_with(dec(value), RoundingMode::HalfEven)
                .unwrap(),
            dec(expected)
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_065_up_and_down_directional_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tenth = currency(&mut tx, "TEN", "0.1").await;
    let rounding = rounding_of(&mut tx, tenth).await;
    assert_eq!(
        rounding
            .round_with(dec("1.21"), RoundingMode::AwayFromZero)
            .unwrap(),
        dec("1.3")
    );
    assert_eq!(
        rounding
            .round_with(dec("1.21"), RoundingMode::TowardZero)
            .unwrap(),
        dec("1.2")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_067_is_zero_threshold_detection() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let rounding = rounding_of(&mut tx, usd).await;
    assert!(rounding.is_zero(dec("0.004")).unwrap());
    assert!(!rounding.is_zero(dec("0.01")).unwrap());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_068_compare_amounts_is_ternary_after_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let rounding = rounding_of(&mut tx, usd).await;
    assert_eq!(
        rounding
            .compare_amounts(dec("1.432"), dec("1.431"))
            .unwrap(),
        Ordering::Equal
    );
    assert_eq!(
        rounding
            .compare_amounts(dec("1.432"), dec("1.441"))
            .unwrap(),
        Ordering::Less
    );
    assert_eq!(
        rounding
            .compare_amounts(dec("1.449"), dec("1.431"))
            .unwrap(),
        Ordering::Greater
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_069_is_zero_of_a_difference_is_not_compare_equal() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let rounding = rounding_of(&mut tx, usd).await;
    let (a, b) = (dec("0.006"), dec("0.002"));
    assert!(rounding.is_zero(a - b).unwrap());
    assert_ne!(rounding.compare_amounts(a, b).unwrap(), Ordering::Equal);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_071_currency_round_uses_its_own_increment() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let jpy = currency(&mut tx, "JPY", "1.0").await;
    let rounding = rounding_of(&mut tx, jpy).await;
    let rounded = rounding.round(dec("123.45")).unwrap();
    assert_eq!(rounded, dec("123.45").round_dp(0));
    assert_eq!(rounded.scale(), 0);
    // a cash increment that is not a power of ten
    let swiss = currency(&mut tx, "CHF", "0.05").await;
    let cash = rounding_of(&mut tx, swiss).await;
    assert_eq!(cash.round(dec("1.03")).unwrap(), dec("1.05"));
    assert_eq!(cash.round(dec("1.02")).unwrap(), dec("1.00"));
    assert_eq!(cash.decimal_places(), 2);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the automatic rounding of monetary values on create and write is not wired into the ORM write
/// path by this slice)
#[tokio::test]
async fn monetary_amounts_round_to_each_currency_increment() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let amount = dec("14.70126");
    let cases = [
        ("C01", "0.01", "14.70"),
        ("C02", "0.0001", "14.7013"),
        ("C03", "1.0", "15"),
    ];
    for (code, increment, expected) in cases {
        let id = currency(&mut tx, code, increment).await;
        let rounding = rounding_of(&mut tx, id).await;
        assert_eq!(rounding.round(amount).unwrap(), dec(expected), "{code}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// the rate table
// ---------------------------------------------------------------------------------------------

/// rates included, and a strictly positive rate)
#[tokio::test]
async fn rate_table_constraints() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let records = tx.env().records("res.currency.rate").unwrap();
    rate(&mut tx, eur, Some(root), "2026-01-01", "1.10").await;
    rate(&mut tx, eur, None, "2026-01-01", "1.20").await;

    for company in [Some(root), None] {
        let error = tx
            .savepoint(async |tx| {
                records
                    .create(tx, vec![rate_vals(eur, company, "2026-01-01", "1.50")])
                    .await?;
                tx.flush().await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(
            error
                .message()
                .ends_with("Only one currency rate per day allowed!"),
            "{}",
            error.message()
        );
    }
    for bad in ["0", "-1.5"] {
        let error = tx
            .savepoint(async |tx| {
                records
                    .create(tx, vec![rate_vals(eur, Some(root), "2026-02-01", bad)])
                    .await?;
                tx.flush().await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(
            error
                .message()
                .ends_with("The currency rate must be strictly positive."),
            "{}",
            error.message()
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rate_numeric_round_trip_is_exact() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let precise = dec("0.1234567890123456789012345678");
    let id = rate(
        &mut tx,
        eur,
        Some(root),
        "2026-01-01",
        &canonical_string(precise),
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let stored = tx
        .env()
        .records("res.currency.rate")
        .unwrap()
        .browse([id.get()])
        .unwrap()
        .values(&mut tx, "rate")
        .await
        .unwrap();
    assert_eq!(stored, vec![Value::Decimal(precise)]);
    let looked_up = book(
        &mut tx,
        root,
        "2026-06-01",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(
        looked_up
            .snapshot(usd, eur)
            .unwrap()
            .rates()
            .unwrap()
            .1
            .rate(),
        precise
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// access
// ---------------------------------------------------------------------------------------------

fn company_id(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

async fn begin_user(h: &Harness, uid: i64, companies: &[i64]) -> Transaction {
    let resolution = Resolution::in_id_order(
        GroupSet::default(),
        companies.iter().map(|c| company_id(*c)),
        NonZeroU64::MIN,
    )
    .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(resolution)
        .companies(companies.iter().map(|c| company_id(*c)).collect())
        .build()
        .unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal).context(Context::new()))
        .await
        .unwrap()
}

/// access to the rate table)
#[tokio::test]
async fn rate_lookup_is_access_checked() {
    let denied = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.currency.rate", Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(denied)
        .build()
        .await
        .unwrap();
    crate::rate_type_fixture::seed_rate_types(&h).await;
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "USD", "0.01").await;
    let eur = currency(&mut setup, "EUR", "0.01").await;
    let own = company(&mut setup, "Own", usd, None).await;
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Rater', 'rater', true, false)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let mut tx = begin_user(&h, 42, &[own.get()]).await;
    let env: Env = tx.env();
    let error = RateBook::load(
        &env,
        &mut tx,
        own,
        day("2026-06-01"),
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Access,
        "no read access to the rate table: {error}"
    );
    drop(tx);

    // with read access, a foreign company is still refused
    let open = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    crate::rate_type_fixture::seed_rate_types(&open).await;
    let mut setup = open.begin().await.unwrap();
    let usd = currency(&mut setup, "USD", "0.01").await;
    let eur = currency(&mut setup, "EUR", "0.01").await;
    let own = company(&mut setup, "Own", usd, None).await;
    let foreign_company = company(&mut setup, "Foreign", usd, None).await;
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Rater', 'rater', true, false)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = begin_user(&open, 42, &[own.get()]).await;
    let env: Env = tx.env();
    let error = RateBook::load(
        &env,
        &mut tx,
        foreign_company,
        day("2026-06-01"),
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "One of the selected companies does not exist or is not one of yours."
    );
    drop(tx);
    open.cleanup().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// decimal precision
// ---------------------------------------------------------------------------------------------

async fn precision_row(tx: &mut Transaction, usage: &str, digits: i64) -> erp_sdk::Result<()> {
    let vals = RawVals::new()
        .set_cell("name", text_cell(usage))
        .set_cell("digits", Cell::Int(digits as i32));
    let records = tx.env().records("decimal.precision").unwrap();
    records.create(tx, vec![vals]).await?;
    tx.flush().await
}

#[tokio::test]
async fn spec_base_072_precision_get_resolution() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    precision_row(&mut tx, "Product Price", 4).await.unwrap();
    assert_eq!(
        precision_get(&env, &mut tx, "Product Price").await.unwrap(),
        4
    );
    assert_eq!(
        precision_defined(&env, &mut tx, "Product Price")
            .await
            .unwrap(),
        Some(4)
    );
    assert_eq!(
        precision_get(&env, &mut tx, "Never Configured")
            .await
            .unwrap(),
        DEFAULT_PRECISION_DIGITS
    );
    assert_eq!(
        precision_defined(&env, &mut tx, "Never Configured")
            .await
            .unwrap(),
        None
    );
    for bad in ["", &"x".repeat(129)] {
        let error = precision_get(&env, &mut tx, bad).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn precision_lookup_works_for_ordinary_users() {
    let closed = DomainPolicy::builder(Otherwise::Allow)
        .rule("decimal.precision", Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(closed)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    precision_row(&mut setup, "Product Price", 4).await.unwrap();
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Clerk', 'clerk', true, false)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut user = begin_user(&h, 42, &[1]).await;
    let env: Env = user.env();
    assert!(!env.su());
    // the table itself stays closed to this user ...
    let direct = env
        .records("decimal.precision")
        .unwrap()
        .check_access(&mut user, Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(direct.kind(), ErrorKind::Access);
    // ... while the lookup answers
    assert_eq!(
        precision_get(&env, &mut user, "Product Price")
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        precision_defined(&env, &mut user, "Other").await.unwrap(),
        None
    );
    drop(user);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn precision_table_rules() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    precision_row(&mut tx, "Account", 2).await.unwrap();
    let duplicate = tx
        .savepoint(async |tx| precision_row(tx, "Account", 3).await)
        .await
        .unwrap_err();
    assert_eq!(duplicate.kind(), ErrorKind::Validation);
    assert!(
        duplicate
            .message()
            .ends_with("Only one value can be defined for each given usage!"),
        "{}",
        duplicate.message()
    );
    for digits in [-1, 29] {
        let error = tx
            .savepoint(async |tx| precision_row(tx, "Wide", digits).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{digits}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `fields_get` cache of the engine is not changed by this slice)
#[tokio::test]
async fn field_scale_resolves_fixed_named_unrounded_and_monetary() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    // fixed digits=(12, 6), unrounded digits=0
    assert_eq!(
        decimal_precision::field_scale(&env, &mut tx, "res.currency", "rounding")
            .await
            .unwrap(),
        FieldScale::Places(6)
    );
    assert_eq!(
        decimal_precision::field_scale(&env, &mut tx, "res.currency.rate", "rate")
            .await
            .unwrap(),
        FieldScale::Unrounded
    );
    // named: the table decides, the default when nobody configured it
    assert_eq!(
        decimal_precision::field_scale(&env, &mut tx, "test_defaults.item", "price")
            .await
            .unwrap(),
        FieldScale::Places(DEFAULT_PRECISION_DIGITS)
    );
    precision_row(&mut tx, "Product Price", 4).await.unwrap();
    assert_eq!(
        decimal_precision::field_scale(&env, &mut tx, "test_defaults.item", "price")
            .await
            .unwrap(),
        FieldScale::Places(4)
    );
    // monetary follows its currency field
    assert_eq!(
        decimal_precision::field_scale(&env, &mut tx, "test_orm.mixed", "monetary")
            .await
            .unwrap(),
        FieldScale::Currency {
            currency_field: "currency_id".to_owned()
        }
    );
    // a field with no decimal notion is a programming error
    for (model, field) in [
        ("res.currency", "name"),
        ("res.currency", "nope"),
        ("no.such", "x"),
    ] {
        let error = decimal_precision::field_scale(&env, &mut tx, model, field)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming, "{model}.{field}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// dated parameters (H35)
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments, reason = "a test row builder")]
fn param_vals(
    key: &str,
    company: Option<RecId>,
    from: &str,
    to: Option<&str>,
    kind: &str,
    value: &str,
) -> RawVals {
    let mut vals = RawVals::new()
        .set_cell("key", text_cell(key))
        .set_cell("date_from", Cell::Date(day(from)))
        .set_cell("value_kind", text_cell(kind))
        .set_cell("value", text_cell(value));
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company.get()));
    }
    if let Some(to) = to {
        vals = vals.set_cell("date_to", Cell::Date(day(to)));
    }
    vals
}

async fn param(
    tx: &mut Transaction,
    key: &str,
    company: Option<RecId>,
    from: &str,
    to: Option<&str>,
    kind: &str,
    value: &str,
) -> erp_sdk::Result<()> {
    let records = tx.env().records("ir.dated.parameter").unwrap();
    records
        .create(tx, vec![param_vals(key, company, from, to, kind, value)])
        .await?;
    tx.flush().await
}

#[tokio::test]
async fn spec_base_028_dated_parameter_resolution() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let co = company(&mut tx, "RootCorp", usd, None).await;
    let key = "payroll.pension_rate";
    let (old, new) = ("0.11", "0.12");
    param(
        &mut tx,
        key,
        None,
        "2026-01-01",
        Some("2026-12-31"),
        "decimal",
        old,
    )
    .await
    .unwrap();
    param(&mut tx, key, None, "2027-01-01", None, "decimal", new)
        .await
        .unwrap();
    let env = tx.env();

    let mid_2026 = dated_parameter::resolve(&env, &mut tx, key, co, day("2026-06-15"))
        .await
        .unwrap();
    assert_eq!(mid_2026.value, ParamValue::Decimal(dec(old)));
    // both ends of a period are inclusive, and the next period starts the next day
    let last_day = dated_parameter::resolve(&env, &mut tx, key, co, day("2026-12-31"))
        .await
        .unwrap();
    assert_eq!(last_day.value, ParamValue::Decimal(dec(old)));
    let first_day = dated_parameter::resolve(&env, &mut tx, key, co, day("2027-01-01"))
        .await
        .unwrap();
    assert_eq!(first_day.value, ParamValue::Decimal(dec(new)));
    let open_ended = dated_parameter::resolve(&env, &mut tx, key, co, day("2040-01-01"))
        .await
        .unwrap();
    assert_eq!(open_ended.value, ParamValue::Decimal(dec(new)));
    assert_eq!(open_ended.date_to, None);
    // before the first period nothing is in force: loud, with no default
    let error = dated_parameter::resolve(&env, &mut tx, key, co, day("2025-12-31"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains(key), "{}", error.message());
    assert!(
        error.message().contains("2025-12-31"),
        "{}",
        error.message()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the shared one; one bulk read serves many keys, typed)
#[tokio::test]
async fn dated_parameter_company_scope_and_bulk_snapshot() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let a = company(&mut tx, "A", usd, None).await;
    let b = company(&mut tx, "B", usd, None).await;
    param(
        &mut tx,
        "tax.vat_rate",
        None,
        "2026-01-01",
        None,
        "decimal",
        "0.11",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.vat_rate",
        Some(a),
        "2026-01-01",
        Some("2026-12-31"),
        "decimal",
        "0.10",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.round_cash",
        None,
        "2026-01-01",
        None,
        "boolean",
        "true",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.max_lines",
        None,
        "2026-01-01",
        None,
        "integer",
        "500",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.label",
        None,
        "2026-01-01",
        None,
        "text",
        "VAT",
    )
    .await
    .unwrap();
    let env = tx.env();
    let keys = [
        "tax.vat_rate",
        "tax.round_cash",
        "tax.max_lines",
        "tax.label",
    ];

    let for_a = dated_parameter::resolve_many(&env, &mut tx, &keys, a, day("2026-06-15"))
        .await
        .unwrap();
    assert_eq!(for_a.decimal("tax.vat_rate").unwrap(), dec("0.10"));
    assert!(for_a.boolean("tax.round_cash").unwrap());
    assert_eq!(for_a.integer("tax.max_lines").unwrap(), 500);
    assert_eq!(for_a.text("tax.label").unwrap(), "VAT");
    assert_eq!(for_a.get("tax.vat_rate").unwrap().company_id, Some(a.get()));
    assert_eq!(for_a.get("tax.label").unwrap().company_id, None);

    let for_b = dated_parameter::resolve_many(&env, &mut tx, &keys, b, day("2026-06-15"))
        .await
        .unwrap();
    assert_eq!(for_b.decimal("tax.vat_rate").unwrap(), dec("0.11"));
    // after A's own period ended, A is back on the shared value
    let later = dated_parameter::resolve_many(&env, &mut tx, &keys, a, day("2027-03-01"))
        .await
        .unwrap();
    assert_eq!(later.decimal("tax.vat_rate").unwrap(), dec("0.11"));

    // the entries are what a rule binds as variables
    let mut names: Vec<&str> = for_a.entries().map(|(k, _)| k).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "tax.label",
            "tax.max_lines",
            "tax.round_cash",
            "tax.vat_rate"
        ]
    );
    assert!(
        for_a
            .entries()
            .any(|(k, v)| k == "tax.round_cash" && v.kind() == ParamKind::Boolean)
    );

    // a wrong type and a key nobody defined are errors
    assert_eq!(
        for_a.integer("tax.vat_rate").unwrap_err().kind(),
        ErrorKind::Validation
    );
    assert_eq!(
        for_a.get("tax.other").unwrap_err().kind(),
        ErrorKind::UserError
    );
    let missing = dated_parameter::resolve_many(
        &env,
        &mut tx,
        &["tax.vat_rate", "tax.absent"],
        a,
        day("2026-06-15"),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::UserError);
    assert!(
        missing.message().contains("tax.absent"),
        "{}",
        missing.message()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dated_parameter_periods_cannot_overlap() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let a = company(&mut tx, "A", usd, None).await;
    let b = company(&mut tx, "B", usd, None).await;
    param(
        &mut tx,
        "tax.vat_rate",
        None,
        "2026-01-01",
        Some("2026-12-31"),
        "decimal",
        "0.11",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.vat_rate",
        Some(a),
        "2026-01-01",
        Some("2026-12-31"),
        "decimal",
        "0.10",
    )
    .await
    .unwrap();

    // the same scope overlaps on a shared day (inclusive ends) or inside an open-ended period
    for (company, from, to) in [
        (None, "2026-12-31", None),
        (None, "2026-06-01", Some("2026-06-30")),
        (Some(a), "2026-12-31", Some("2027-06-30")),
        (Some(a), "2025-01-01", None),
    ] {
        let error = tx
            .savepoint(async |tx| {
                param(tx, "tax.vat_rate", company, from, to, "decimal", "0.50").await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{company:?} {from}");
        assert!(
            error.message().contains("overlapping period"),
            "{}",
            error.message()
        );
    }
    // different scope, different key, or the day after: fine
    param(
        &mut tx,
        "tax.vat_rate",
        Some(b),
        "2026-06-01",
        None,
        "decimal",
        "0.09",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.other_rate",
        None,
        "2026-06-01",
        None,
        "decimal",
        "0.01",
    )
    .await
    .unwrap();
    param(
        &mut tx,
        "tax.vat_rate",
        None,
        "2027-01-01",
        None,
        "decimal",
        "0.12",
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// shape, exact decimals survive)
#[tokio::test]
async fn dated_parameter_input_validation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let invalid = [
        ("tax.rate", "decimal", "1e3"),
        ("tax.rate", "decimal", "0.5 "),
        ("tax.rate", "decimal", ""),
        ("tax.count", "integer", "1.5"),
        ("tax.count", "integer", "007"),
        ("tax.flag", "boolean", "True"),
        ("Tax.Rate", "decimal", "1"),
        ("tax..rate", "decimal", "1"),
        ("tax.rate.", "decimal", "1"),
        ("", "decimal", "1"),
    ];
    for (key, kind, value) in invalid {
        let error = tx
            .savepoint(async |tx| param(tx, key, None, "2026-01-01", None, kind, value).await)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::Validation,
            "{key} {kind} {value:?}: {error}"
        );
    }
    let too_long = "x".repeat(1025);
    let error = tx
        .savepoint(async |tx| {
            param(tx, "tax.text", None, "2026-01-01", None, "text", &too_long).await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    // an end before the start
    let error = tx
        .savepoint(async |tx| {
            param(
                tx,
                "tax.rate",
                None,
                "2026-06-01",
                Some("2026-05-31"),
                "decimal",
                "1",
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);

    // 28 decimals round trip through the table and the reader without a float
    let usd = currency(&mut tx, "USD", "0.01").await;
    let co = company(&mut tx, "RootCorp", usd, None).await;
    let precise = "0.1234567890123456789012345678";
    param(
        &mut tx,
        "tax.precise",
        None,
        "2026-01-01",
        None,
        "decimal",
        precise,
    )
    .await
    .unwrap();
    let env = tx.env();
    let got = dated_parameter::resolve(&env, &mut tx, "tax.precise", co, day("2026-06-01"))
        .await
        .unwrap();
    assert_eq!(got.value, ParamValue::Decimal(dec(precise)));

    // reader input bounds
    let no_keys = dated_parameter::resolve_many(&env, &mut tx, &[], co, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(no_keys.kind(), ErrorKind::Validation);
    let bad_key = dated_parameter::resolve_many(&env, &mut tx, &["Bad Key"], co, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(bad_key.kind(), ErrorKind::Validation);
    let many: Vec<String> = (0..300).map(|i| format!("tax.k{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let too_many = dated_parameter::resolve_many(&env, &mut tx, &refs, co, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(too_many.kind(), ErrorKind::Validation);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the caller's companies)
#[tokio::test]
async fn dated_parameter_reader_is_access_checked() {
    let denied = DomainPolicy::builder(Otherwise::Allow)
        .rule("ir.dated.parameter", Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(denied)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let usd = currency(&mut setup, "USD", "0.01").await;
    let own = company(&mut setup, "Own", usd, None).await;
    param(
        &mut setup,
        "tax.vat_rate",
        None,
        "2026-01-01",
        None,
        "decimal",
        "0.11",
    )
    .await
    .unwrap();
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Reader', 'reader', true, false)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    // the superuser reads; a user the policy denies does not, and never gets an empty answer
    let mut system = h.begin().await.unwrap();
    let env = system.env();
    assert!(
        dated_parameter::resolve(&env, &mut system, "tax.vat_rate", own, day("2026-06-01"))
            .await
            .is_ok()
    );
    drop(system);
    let mut user = begin_user(&h, 42, &[own.get()]).await;
    let env: Env = user.env();
    let error = dated_parameter::resolve(&env, &mut user, "tax.vat_rate", own, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    drop(user);

    // the model-level right is checked first (the company check has its own tests in
    // `base_money_review.rs`), so a foreign company on a closed model reports the closed model
    let mut user = begin_user(&h, 42, &[own.get() + 1000]).await;
    let env: Env = user.env();
    let error = dated_parameter::resolve(&env, &mut user, "tax.vat_rate", own, day("2026-06-01"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you read 'Dated Parameter' (ir.dated.parameter) records."
    );
    drop(user);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// rate types
// ---------------------------------------------------------------------------------------------

async fn type_id(tx: &mut Transaction, code: &str) -> RecId {
    let rows = tx
        .query(
            "SELECT id FROM res_currency_rate_type WHERE code = $1",
            &[&code],
        )
        .await
        .unwrap();
    RecId::from_real(rows[0].get::<_, i64>(0)).unwrap()
}

async fn typed_rate(
    tx: &mut Transaction,
    code: &str,
    currency: RecId,
    company: Option<RecId>,
    on: &str,
    value: &str,
) -> erp_sdk::Result<RecId> {
    let rate_type = type_id(tx, code).await;
    let vals =
        rate_vals(currency, company, on, value).set_cell("rate_type_id", Cell::Id(rate_type.get()));
    let records = tx.env().records("res.currency.rate").unwrap();
    Ok(records.create(tx, vec![vals]).await?.rec_ids()[0])
}

async fn typed_book(
    tx: &mut Transaction,
    company: RecId,
    on: &str,
    code: &str,
    currencies: &[RecId],
) -> erp_sdk::Result<RateBook> {
    let env = tx.env();
    RateBook::load_typed(&env, tx, company, day(on), code, currencies).await
}

#[tokio::test]
async fn rate_key_includes_the_type_and_untyped_rates_are_spot() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    typed_rate(&mut tx, "spot", eur, Some(root), "2026-01-01", "1.10")
        .await
        .unwrap();
    typed_rate(&mut tx, "tax", eur, Some(root), "2026-01-01", "1.20")
        .await
        .unwrap();
    let plain = rate(&mut tx, eur, Some(root), "2026-02-01", "1.30").await;
    let spot = type_id(&mut tx, "spot").await;
    let rows = tx
        .query(
            "SELECT rate_type_id FROM res_currency_rate WHERE id = $1",
            &[&plain.get()],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), spot.get());

    let error = tx
        .savepoint(async |tx| {
            typed_rate(tx, "tax", eur, Some(root), "2026-01-01", "1.50").await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(
        error
            .message()
            .ends_with("Only one currency rate per day allowed!"),
        "{}",
        error.message()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn typed_lookup_stays_inside_its_type() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    typed_rate(&mut tx, "spot", eur, Some(root), "2026-01-01", "0.90")
        .await
        .unwrap();
    typed_rate(&mut tx, "spot", eur, None, "2026-01-02", "0.80")
        .await
        .unwrap();
    let tax_root = typed_rate(&mut tx, "tax", eur, Some(root), "2026-01-01", "0.50")
        .await
        .unwrap();
    typed_rate(&mut tx, "tax", eur, None, "2026-01-02", "0.40")
        .await
        .unwrap();

    let tax = typed_book(&mut tx, root, "2026-01-10", "tax", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(tax.rate_type().code(), "tax");
    let fx = tax.snapshot(usd, eur).unwrap();
    let (_, to) = fx.rates().unwrap();
    // the root company's tax row wins over the newer shared tax row, and no spot row is read
    assert_eq!(to.rate_id(), Some(tax_root.get()));
    assert_eq!(to.rate(), dec("0.50"));

    let spot = typed_book(&mut tx, root, "2026-01-10", "spot", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(spot.rate_type().code(), "spot");
    assert_eq!(
        spot.snapshot(usd, eur).unwrap().rates().unwrap().1.rate(),
        dec("0.90")
    );

    let tax_type = type_id(&mut tx, "tax").await.get();
    tx.execute(
        "DELETE FROM res_currency_rate WHERE rate_type_id = $1",
        &[&tax_type],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let tax = typed_book(&mut tx, root, "2026-01-10", "tax", &[usd, eur])
        .await
        .unwrap();
    let error = tax.snapshot(usd, eur).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(
        error.message().ends_with("Rate type: tax."),
        "{}",
        error.message()
    );
    assert!(error.message().contains("EUR"), "{}", error.message());

    let unknown = typed_book(&mut tx, root, "2026-01-10", "nope", &[usd])
        .await
        .unwrap_err();
    assert_eq!(unknown.kind(), ErrorKind::Missing);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn date_rule_and_fallback_come_from_the_type() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    // tax values differ from spot's, so an untyped call that read tax rows would show
    for (code, first, second) in [("spot", "2.0", "4.0"), ("tax", "20.0", "40.0")] {
        typed_rate(&mut tx, code, eur, Some(root), "2026-03-01", first)
            .await
            .unwrap();
        typed_rate(&mut tx, code, eur, Some(root), "2026-03-02", second)
            .await
            .unwrap();
    }
    let used = |book: &RateBook| {
        let fx = book.snapshot(usd, eur).unwrap();
        let (_, to) = fx.rates().unwrap();
        (to.rate(), to.origin())
    };
    // the untyped call: its explicit rule, unchanged
    let before = book(
        &mut tx,
        root,
        "2026-03-02",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(used(&before), (dec("2.0"), RateOrigin::Preceding));
    let on = book(
        &mut tx,
        root,
        "2026-03-02",
        RateDateRule::OnOrBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(used(&on), (dec("4.0"), RateOrigin::Preceding));
    // the typed call: spot is strictly before, tax is on or before
    let spot = typed_book(&mut tx, root, "2026-03-02", "spot", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(used(&spot), (dec("2.0"), RateOrigin::Preceding));
    let tax = typed_book(&mut tx, root, "2026-03-02", "tax", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(used(&tax), (dec("40.0"), RateOrigin::Preceding));
    // before the first row: spot falls back to the earliest later row, tax reports it missing
    let spot = typed_book(&mut tx, root, "2026-02-01", "spot", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(used(&spot), (dec("2.0"), RateOrigin::LaterFallback));
    let tax = typed_book(&mut tx, root, "2026-02-01", "tax", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(
        tax.snapshot(usd, eur).unwrap_err().kind(),
        ErrorKind::UserError
    );
    // changing the type's rule changes the typed result only
    tx.execute(
        "UPDATE res_currency_rate_type SET date_rule = 'strictly_before' WHERE code = 'tax'",
        &[],
    )
    .await
    .unwrap();
    let tax = typed_book(&mut tx, root, "2026-03-02", "tax", &[usd, eur])
        .await
        .unwrap();
    assert_eq!(used(&tax), (dec("20.0"), RateOrigin::Preceding));
    let on = book(
        &mut tx,
        root,
        "2026-03-02",
        RateDateRule::OnOrBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(used(&on), (dec("4.0"), RateOrigin::Preceding));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// asserted against the real data file in `base_currency_language_data.rs`)
#[tokio::test]
async fn rate_type_code_is_unique_and_a_used_type_cannot_be_deleted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let types = tx.env().records("res.currency.rate.type").unwrap();
    let error = tx
        .savepoint(async |tx| {
            types
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("code", text_cell("tax"))
                            .set_cell("name", text_cell("Another tax")),
                    ],
                )
                .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(
        error
            .message()
            .ends_with("The rate type code must be unique!"),
        "{}",
        error.message()
    );
    typed_rate(&mut tx, "tax", eur, Some(root), "2026-01-01", "1.5")
        .await
        .unwrap();
    let tax = type_id(&mut tx, "tax").await;
    let error = tx
        .savepoint(async |tx| {
            types.browse([tax.get()])?.unlink(tx).await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{}", error.message());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// Default-type lifecycle protection.
async fn rate_type_state(tx: &mut Transaction) -> Vec<(i64, String, String, Option<bool>, i32)> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT id, code, name, active, sequence FROM res_currency_rate_type ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
    .collect()
}

#[tokio::test]
async fn spot_protection_refuses_code_changes_and_archive() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let spot = type_id(&mut tx, "spot").await;
    let records = tx
        .env()
        .records("res.currency.rate.type")
        .unwrap()
        .browse([spot.get()])
        .unwrap();
    let before = rate_type_state(&mut tx).await;
    for code in [text_cell("renamed"), Cell::Null] {
        let error = records
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("code", code)
                    .set_cell("active", Cell::Bool(false))
                    .set_cell("name", text_cell("Changed")),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "The code of the default currency rate type 'spot' cannot be changed."
        );
        assert_eq!(rate_type_state(&mut tx).await, before);
    }
    for active in [Cell::Bool(false), Cell::Null] {
        let error = records
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("active", active)
                    .set_cell("name", text_cell("Changed")),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "The default currency rate type 'spot' cannot be archived."
        );
        assert_eq!(rate_type_state(&mut tx).await, before);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spot_protection_refuses_delete_without_rates() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let spot = type_id(&mut tx, "spot").await;
    let before = rate_type_state(&mut tx).await;
    let error = tx
        .env()
        .records("res.currency.rate.type")
        .unwrap()
        .browse([spot.get()])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "The default currency rate type 'spot' cannot be deleted."
    );
    assert_eq!(rate_type_state(&mut tx).await, before);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spot_protection_preflights_the_entire_batch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let spot = type_id(&mut tx, "spot").await;
    let tax = type_id(&mut tx, "tax").await;
    let types = tx.env().records("res.currency.rate.type").unwrap();
    let before = rate_type_state(&mut tx).await;
    for ids in [
        vec![tax.get(), spot.get()],
        vec![spot.get(), tax.get()],
        vec![tax.get(), spot.get(), tax.get()],
    ] {
        let records = types.browse(ids).unwrap();
        for (field, value, message) in [
            (
                "code",
                text_cell("renamed"),
                "The code of the default currency rate type 'spot' cannot be changed.",
            ),
            (
                "active",
                Cell::Bool(false),
                "The default currency rate type 'spot' cannot be archived.",
            ),
        ] {
            let error = records
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell(field, value)
                        .set_cell("name", text_cell("Changed")),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UserError);
            assert_eq!(error.message(), message);
            assert_eq!(rate_type_state(&mut tx).await, before);
        }
        let error = records.unlink(&mut tx).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "The default currency rate type 'spot' cannot be deleted."
        );
        assert_eq!(rate_type_state(&mut tx).await, before);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spot_protection_preserves_allowed_operations_and_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let types = tx.env().records("res.currency.rate.type").unwrap();
    let spot = type_id(&mut tx, "spot").await;
    let records = types.browse([spot.get()]).unwrap();
    records
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("code", text_cell("spot"))
                .set_cell("active", Cell::Bool(true))
                .set_cell("name", text_cell("Default Spot"))
                .set_cell("sequence", Cell::Int(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        records.value(&mut tx, "name").await.unwrap(),
        Value::str("Default Spot")
    );
    assert_eq!(
        records.value(&mut tx, "sequence").await.unwrap(),
        Value::Int(5)
    );
    let empty = types.browse([]).unwrap();
    empty
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("code", text_cell("renamed"))
                .set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    empty.unlink(&mut tx).await.unwrap();

    let custom = types
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("code", text_cell("custom"))
                    .set_cell("name", text_cell("Custom")),
            ],
        )
        .await
        .unwrap();
    custom
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("code", text_cell("custom_renamed"))
                .set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let archived = money::RateTypeRef::by_code(&mut tx, "custom_renamed")
        .await
        .unwrap();
    assert_eq!(archived.id(), custom.ids()[0]);
    custom
        .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    custom.unlink(&mut tx).await.unwrap();
    assert!(custom.exists(&mut tx).await.unwrap().is_empty());

    let error = records
        .write(&mut tx, RawVals::new().set_cell("code", text_cell("gone")))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "The code of the default currency rate type 'spot' cannot be changed."
    );
    let usd = currency(&mut tx, "USD", "0.01").await;
    let eur = currency(&mut tx, "EUR", "0.01").await;
    let root = company(&mut tx, "RootCorp", usd, None).await;
    let plain = rate(&mut tx, eur, Some(root), "2026-01-01", "2.0").await;
    typed_rate(&mut tx, "tax", eur, Some(root), "2026-01-01", "20.0")
        .await
        .unwrap();
    let rates = tx
        .env()
        .records("res.currency.rate")
        .unwrap()
        .browse([plain.get()])
        .unwrap();
    assert_eq!(
        rates.value(&mut tx, "rate_type_id").await.unwrap(),
        Value::Int(spot.get())
    );
    let loaded = book(
        &mut tx,
        root,
        "2026-01-02",
        RateDateRule::StrictlyBefore,
        &[usd, eur],
    )
    .await
    .unwrap();
    assert_eq!(loaded.rate_type().id(), spot.get());
    assert_eq!(
        loaded.snapshot(usd, eur).unwrap().rates().unwrap().1.rate(),
        dec("2.0")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spot_protection_preserves_access_checks() {
    let denied = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.currency.rate.type", Operation::Write, Domain::FALSE)
        .rule("res.currency.rate.type", Operation::Unlink, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(denied)
        .build()
        .await
        .unwrap();
    rate_type_fixture::seed_rate_types(&h).await;
    let mut setup = h.begin().await.unwrap();
    let spot = type_id(&mut setup, "spot").await;
    setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Reader', 'reader', true, false)")
        .await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = begin_user(&h, 42, &[]).await;
    let records = tx
        .env()
        .records("res.currency.rate.type")
        .unwrap()
        .browse([spot.get()])
        .unwrap();
    let error = records
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you write 'Currency Rate Type' (res.currency.rate.type) records."
    );
    let error = records.unlink(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you unlink 'Currency Rate Type' (res.currency.rate.type) records."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
