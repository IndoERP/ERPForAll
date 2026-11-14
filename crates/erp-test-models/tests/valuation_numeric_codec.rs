// SPDX-License-Identifier: MIT OR Apache-2.0
//! Numeric codec/check prerequisites, exercised on isolated tables without activating valuation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "database assertions"
)]
use erp_fields::money::{CanonicalFraction, CanonicalInteger, GridValue, Increment, parse_decimal};
use erp_sql::exact_numeric::{carry_check, fraction_check, grid_check};
use erp_testkit::{
    TestDb,
    tokio_postgres::{Client, types::ToSql},
};

#[tokio::test]
async fn wide_grid_and_fraction_survive_commit_and_another_session() {
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    client
        .batch_execute("CREATE TABLE numeric_roundtrip (value numeric, num numeric, den numeric)")
        .await
        .unwrap();
    let value = GridValue::parse("79228162514264337593543950336.000000000000000001", 18).unwrap();
    let pair = CanonicalFraction::parse("-2", "3").unwrap();
    client.batch_execute("BEGIN").await.unwrap();
    client
        .execute(
            "INSERT INTO numeric_roundtrip VALUES ($1::numeric,$2::numeric,$3::numeric)",
            &[&value, &pair.numerator(), &pair.denominator()],
        )
        .await
        .unwrap();
    client.batch_execute("COMMIT").await.unwrap();
    let reader = db.client().await.unwrap();
    let row = reader
        .query_one(
            "SELECT value,num,den,value::text,num::text,den::text FROM numeric_roundtrip",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, GridValue>(0).rescale(18).unwrap(), value);
    assert_eq!(
        CanonicalFraction::new(
            row.get::<_, CanonicalInteger>(1),
            row.get::<_, CanonicalInteger>(2)
        )
        .unwrap(),
        pair
    );
    assert_eq!(
        row.get::<_, String>(3),
        "79228162514264337593543950336.000000000000000001"
    );
    assert_eq!(row.get::<_, String>(4), "-2");
    assert_eq!(row.get::<_, String>(5), "3");
    // A stored display scale does not change significant integer or grid precision.
    let row = reader
        .query_one("SELECT 1.000::numeric, 0.000::numeric, 1.000::numeric", &[])
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, GridValue>(0).rescale(18).unwrap().to_string(),
        "1"
    );
    assert_eq!(row.get::<_, GridValue>(1).to_string(), "0");
    assert_eq!(row.get::<_, CanonicalInteger>(2).to_string(), "1");
    drop(reader);
    drop(client);
    db.cleanup().await.unwrap();
}

async fn refused(
    client: &Client,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    table: &str,
    constraint: &str,
) {
    client.batch_execute("BEGIN").await.unwrap();
    let error = client.execute(sql, params).await.unwrap_err();
    let db = error.as_db_error().expect("server constraint error");
    assert_eq!(db.code().code(), "23514");
    assert_eq!(db.constraint(), Some(constraint));
    assert_eq!(
        db.message(),
        format!("new row for relation \"{table}\" violates check constraint \"{constraint}\"")
    );
    client.batch_execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn generated_checks_refuse_bad_rows_without_rounding_or_unsafe_gcd() {
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    client.batch_execute(&format!("CREATE TABLE numeric_grid (value numeric, CONSTRAINT grid_policy {}); CREATE TABLE numeric_optional (value numeric, CONSTRAINT optional_policy {}); CREATE TABLE numeric_carry (carry numeric, increment numeric, CONSTRAINT carry_policy {}); CREATE TABLE numeric_pair (num numeric,den numeric,CONSTRAINT pair_policy {});",grid_check("value",18,false).unwrap(),grid_check("value",18,true).unwrap(),carry_check("carry","increment").unwrap(),fraction_check("num","den").unwrap())).await.unwrap();
    for value in [
        None,
        Some("NaN"),
        Some("Infinity"),
        Some("-Infinity"),
        Some("0.0000000000000000001"),
        Some("1e1000"),
        Some("115792089237316195423570985008687907853269984665640564039457.584007913129639936"),
    ] {
        refused(
            &client,
            "INSERT INTO numeric_grid VALUES ($1::text::numeric)",
            &[&value],
            "numeric_grid",
            "grid_policy",
        )
        .await;
    }
    for value in [
        "0",
        "-0.000000000000000001",
        "1.0000000000000000000000",
        "115792089237316195423570985008687907853269984665640564039457.584007913129639935",
    ] {
        client
            .execute(
                "INSERT INTO numeric_grid VALUES ($1::text::numeric)",
                &[&value],
            )
            .await
            .unwrap();
    }
    client
        .execute("INSERT INTO numeric_optional VALUES (NULL)", &[])
        .await
        .unwrap();
    refused(
        &client,
        "UPDATE numeric_grid SET value='NaN'::numeric",
        &[],
        "numeric_grid",
        "grid_policy",
    )
    .await;
    let count: i64 = client
        .query_one("SELECT count(*) FROM numeric_grid", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 4);
    for (carry, increment) in [
        (Some("0.005000000000000001"), Some("0.01")),
        (Some("-0.005000000000000001"), Some("0.01")),
        (Some("NaN"), Some("0.01")),
        (Some("Infinity"), Some("0.01")),
        (None, Some("0.01")),
        (Some("0"), None),
        (Some("0"), Some("0")),
        (Some("0"), Some("-1")),
        (Some("0"), Some("NaN")),
        (Some("0"), Some("Infinity")),
        (Some("0"), Some("1e1000")),
        (Some("0"), Some("1e-29")),
        (Some("0"), Some("79228162514264337593543950336")),
        (
            Some("0.00000000000000000005"),
            Some("0.0000000000000000001"),
        ),
    ] {
        refused(
            &client,
            "INSERT INTO numeric_carry VALUES ($1::text::numeric,$2::text::numeric)",
            &[&carry, &increment],
            "numeric_carry",
            "carry_policy",
        )
        .await;
    }
    for (carry, increment) in [
        ("-0.005", "0.0100"),
        ("0.005", "0.01"),
        ("0", "1"),
        ("0.025", "0.05"),
        ("0.0000000000000000001", "0.0000000000000000002"),
        (
            "0.0000000000000000000000000001",
            "0.0000000000000000000000000002",
        ),
    ] {
        let inc = Increment::new(parse_decimal(increment).unwrap()).unwrap();
        let carry_value = GridValue::parse(carry, 28)
            .unwrap()
            .validate_carry(&inc)
            .unwrap();
        client
            .execute(
                "INSERT INTO numeric_carry VALUES ($1::numeric,$2::numeric)",
                &[&carry_value, &inc.as_decimal()],
            )
            .await
            .unwrap();
    }
    for (num, den) in [
        (None, Some("1")),
        (Some("1"), None),
        (Some("1"), Some("0")),
        (Some("1"), Some("-3")),
        (Some("0"), Some("2")),
        (Some("2"), Some("6")),
        (Some("1.5"), Some("3")),
        (Some("1"), Some("1.5")),
        (Some("NaN"), Some("1")),
        (Some("1"), Some("Infinity")),
        (Some("1e2000"), Some("1")),
        (Some("1"), Some("1e2000")),
    ] {
        refused(
            &client,
            "INSERT INTO numeric_pair VALUES ($1::text::numeric,$2::text::numeric)",
            &[&num, &den],
            "numeric_pair",
            "pair_policy",
        )
        .await;
    }
    for (num, den) in [("0", "1"), ("-2", "3"), ("2", "3")] {
        client
            .execute(
                "INSERT INTO numeric_pair VALUES ($1::text::numeric,$2::text::numeric)",
                &[&num, &den],
            )
            .await
            .unwrap();
    }
    // Independent decimal arithmetic checks PostgreSQL power and both exact 4096-bit boundaries.
    let bound = power_two_decimal(4096);
    let mut max = bound.clone();
    assert_eq!(max.pop(), Some('6'));
    max.push('5');
    let actual: String = client
        .query_one("SELECT power(2::numeric,4096)::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(actual, bound);
    for (num, den) in [(max.as_str(), "1"), ("1", max.as_str())] {
        let pair = CanonicalFraction::parse(num, den).unwrap();
        client
            .execute(
                "INSERT INTO numeric_pair VALUES ($1::numeric,$2::numeric)",
                &[&pair.numerator(), &pair.denominator()],
            )
            .await
            .unwrap();
        let row = client
            .query_one(
                "SELECT num,den FROM numeric_pair WHERE num=$1::numeric AND den=$2::numeric",
                &[&pair.numerator(), &pair.denominator()],
            )
            .await
            .unwrap();
        assert_eq!(
            CanonicalFraction::new(row.get(0), row.get(1)).unwrap(),
            pair
        );
    }
    for (num, den) in [(bound.as_str(), "1"), ("1", bound.as_str())] {
        refused(
            &client,
            "INSERT INTO numeric_pair VALUES ($1::text::numeric,$2::text::numeric)",
            &[&num, &den],
            "numeric_pair",
            "pair_policy",
        )
        .await;
    }
    refused(
        &client,
        "INSERT INTO numeric_optional VALUES ('NaN'::numeric)",
        &[],
        "numeric_optional",
        "optional_policy",
    )
    .await;
    drop(client);
    db.cleanup().await.unwrap();
}

// Independent test-only decimal digit arithmetic; no production Nat or SQL power is used.
fn power_two_decimal(bits: usize) -> String {
    let mut digits = vec![1_u8];
    for _ in 0..bits {
        let mut carry = 0;
        for digit in &mut digits {
            let next = *digit * 2 + carry;
            *digit = next % 10;
            carry = next / 10;
        }
        if carry != 0 {
            digits.push(carry);
        }
    }
    digits.iter().rev().map(|d| char::from(b'0' + d)).collect()
}
