//! slice D1: the shipped `erp_sql::delta` fragments against PostgreSQL.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::cmp::Ordering;
use std::num::NonZeroU32;

use erp_fields::{Decimal, ErrorKind};
use erp_sdk::Transaction;
use erp_sql::delta::{
    DeltaLimits, GrainColumn, GrainType, GrainValue, MeasureColumn, MeasureType, MeasureValue,
    ProjectionShape, Uuid16,
};
use erp_sql::quote;
use erp_test_models::base_users;
use erp_testkit::Harness;
use time::{Date, Month, OffsetDateTime, UtcOffset};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap()
}

fn shape() -> ProjectionShape {
    ProjectionShape::new(
        "h27.probe",
        NonZeroU32::new(1).unwrap(),
        vec![
            GrainColumn::new("account", GrainType::Int8, true),
            GrainColumn::new("label", GrainType::Text { max_bytes: 32 }, true),
            GrainColumn::new("day", GrainType::Date, true),
            GrainColumn::new("posted", GrainType::Bool, true),
            GrainColumn::new("at", GrainType::TimestampTz, true),
            GrainColumn::new("ident", GrainType::Uuid, true),
        ],
        vec![
            MeasureColumn::new("lines", MeasureType::Int8),
            MeasureColumn::new(
                "amount",
                MeasureType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
        ],
        &DeltaLimits::default(),
    )
    .unwrap()
}

/// `CREATE TABLE` from the shipped fragments; `keyed` adds the grain key. `default_collation`
/// drops `COLLATE "C"` from the column definitions, so only the fragments' own `COLLATE "C"`
/// in `GROUP BY`/`ORDER BY` can produce the canonical text order.
async fn create(
    tx: &mut Transaction,
    s: &ProjectionShape,
    table: &str,
    keyed: bool,
    default_collation: bool,
) {
    let mut parts = s.grain_column_defs();
    if default_collation {
        for part in &mut parts {
            *part = part.replace(" COLLATE \"C\"", "");
            assert!(!part.contains("COLLATE"));
        }
    }
    parts.extend(s.measure_column_defs());
    if keyed {
        parts.push(s.grain_key_definition());
    }
    for (name, check) in s.measure_check_constraints().unwrap() {
        parts.push(format!("CONSTRAINT {} {check}", quote(&name)));
    }
    tx.execute(
        &format!("CREATE TABLE {} ({})", quote(table), parts.join(", ")),
        &[],
    )
    .await
    .unwrap();
}

/// One batch row: account, label, day, posted, instant (unix seconds, display offset in hours),
/// uuid.
struct Row {
    account: Option<i64>,
    label: Option<&'static str>,
    day: Option<(i32, u8, u8)>,
    posted: Option<bool>,
    at: Option<(i64, i8)>,
    ident: Option<u128>,
}

const T0: i64 = 1_704_448_800;

#[allow(clippy::too_many_arguments)]
fn row(
    account: Option<i64>,
    label: Option<&'static str>,
    day: Option<(i32, u8, u8)>,
    posted: Option<bool>,
    at: Option<(i64, i8)>,
    ident: Option<u128>,
) -> Row {
    Row {
        account,
        label,
        day,
        posted,
        at,
        ident,
    }
}

fn date(d: (i32, u8, u8)) -> Date {
    Date::from_calendar_date(d.0, Month::try_from(d.1).unwrap(), d.2).unwrap()
}

fn key_values(r: &Row) -> Vec<GrainValue> {
    vec![
        r.account.map_or(GrainValue::Null, GrainValue::Int8),
        r.label
            .map_or(GrainValue::Null, |t| GrainValue::Text(t.to_owned())),
        r.day
            .map_or(GrainValue::Null, |d| GrainValue::Date(date(d))),
        r.posted.map_or(GrainValue::Null, GrainValue::Bool),
        r.at.map_or(GrainValue::Null, |(secs, hours)| {
            let offset = UtcOffset::from_hms(hours, 0, 0).unwrap();
            GrainValue::TimestampTz(
                OffsetDateTime::from_unix_timestamp(secs)
                    .unwrap()
                    .to_offset(offset),
            )
        }),
        r.ident.map_or(GrainValue::Null, |u| {
            GrainValue::Uuid(Uuid16::from_bytes(u.to_be_bytes()))
        }),
    ]
}

fn uuid_text(u: u128) -> String {
    let h = format!("{u:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn render(v: &GrainValue) -> Option<String> {
    match v {
        GrainValue::Null => None,
        GrainValue::Int8(i) => Some(i.to_string()),
        GrainValue::Bool(b) => Some(b.to_string()),
        GrainValue::Date(d) => Some(d.to_string()),
        GrainValue::Text(t) => Some(t.clone()),
        GrainValue::TimestampTz(t) => Some(format!("{}.000000", t.unix_timestamp())),
        GrainValue::Uuid(u) => Some(uuid_text(u128::from_be_bytes(*u.as_bytes()))),
        other => panic!("unexpected grain value {other:?}"),
    }
}

fn batch() -> Vec<Row> {
    let (a, l, d, p) = (Some(1), Some("a"), Some((2024, 1, 5)), Some(true));
    let (at, id) = (Some((T0, 0)), Some(1u128));
    vec![
        row(a, l, d, p, at, id),
        row(a, l, d, p, at, id),
        // The same instant, shown at another offset, is the same key.
        row(a, l, d, p, Some((T0, 5)), id),
        row(a, l, d, p, Some((T0, -3)), id),
        row(a, l, d, p, Some((T0 + 1, 5)), id),
        row(a, l, d, p, at, Some(u128::MAX)),
        row(
            a,
            l,
            d,
            p,
            at,
            Some(0x0100_0000_0000_0000_0000_0000_0000_0000),
        ),
        // NULL in each grain position.
        row(None, l, d, p, at, id),
        row(a, None, d, p, at, id),
        row(a, l, None, p, at, id),
        row(a, l, d, None, at, id),
        row(a, l, d, p, None, id),
        row(a, l, d, p, at, None),
        row(None, None, None, None, None, None),
        row(None, None, None, None, None, None),
        // Bytewise text order.
        row(a, Some("B"), d, Some(false), at, id),
        row(a, Some("\u{e1}"), d, Some(false), at, id),
        row(a, Some("Z"), d, Some(false), at, id),
        row(a, Some("\u{c4}"), d, Some(false), at, id),
        row(Some(-3), l, Some((1999, 12, 31)), Some(false), at, id),
        row(a, l, d, Some(false), at, id),
    ]
}

/// The per-column text rendering selected by the order test: expressions over the grouped
/// grain expressions, so PostgreSQL accepts them under the same `GROUP BY`.
fn select_list(s: &ProjectionShape, alias: &str) -> String {
    s.grain()
        .iter()
        .zip(s.grain_exprs(alias))
        .map(|(g, e)| match g.ty() {
            GrainType::TimestampTz => format!("(extract(epoch from {e}))::text"),
            _ => format!("({e})::text"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::test]
async fn sql_grouping_and_order_equal_the_rust_canonical_order() {
    let h = harness().await;
    let s = shape();
    let rows = batch();

    // Expected: our keys, sorted and deduplicated by our comparator.
    let mut keys: Vec<_> = rows
        .iter()
        .map(|r| s.admit_key(key_values(r)).unwrap())
        .collect();
    s.sort_keys(&mut keys).unwrap();
    let mut distinct = Vec::new();
    for k in keys {
        let same = match distinct.last() {
            Some(prev) => s.compare(prev, &k).unwrap() == Ordering::Equal,
            None => false,
        };
        if !same {
            distinct.push(k);
        }
    }
    let expected: Vec<Vec<Option<String>>> = distinct
        .iter()
        .map(|k| k.values().iter().map(render).collect())
        .collect();
    assert!(distinct.len() < rows.len(), "the batch repeats keys");

    // The database default collation decides whether the second table can detect a missing
    // COLLATE "C" (it cannot when the database is C or POSIX).
    let mut tx = h.begin().await.unwrap();
    let collate = tx
        .query(
            "SELECT datcollate FROM pg_database WHERE datname = current_database()",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, String>(0);
    eprintln!("delta_sql: datcollate = {collate}");
    tx.rollback().await.unwrap();

    for (table, default_collation) in [("h27_log", false), ("h27_log_default", true)] {
        let mut tx = h.begin().await.unwrap();
        create(&mut tx, &s, table, false, default_collation).await;
        for r in &rows {
            let day = r.day.map(|d| date(d).to_string());
            let epoch = r.at.map(|(secs, _)| secs as f64);
            let ident = r.ident.map(uuid_text);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (account, label, day, posted, at, ident, lines, amount) \
                     VALUES ($1, $2, $3::text::date, $4, to_timestamp($5::float8), \
                     $6::text::uuid, 1, 1)"
                ),
                &[&r.account, &r.label, &day, &r.posted, &epoch, &ident],
            )
            .await
            .unwrap();
        }
        let sql = format!(
            "SELECT {} FROM {table} a GROUP BY {} ORDER BY {}",
            select_list(&s, "a"),
            s.group_by("a"),
            s.order_by("a")
        );
        let got: Vec<Vec<Option<String>>> = tx
            .query(&sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|r| (0..6).map(|i| r.get::<_, Option<String>>(i)).collect())
            .collect();
        assert_eq!(got.len(), distinct.len(), "{table}");
        assert_eq!(got, expected, "{table}");
        tx.rollback().await.unwrap();
    }

    // One NULLS NOT DISTINCT key per grain, NULL included.
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s, "h27_keyed", true, false).await;
    let insert = "INSERT INTO h27_keyed (account, label, day, posted, at, ident, lines, amount) \
                  VALUES (NULL, NULL, NULL, NULL, NULL, NULL, 1, 1)";
    tx.execute(insert, &[]).await.unwrap();
    let e = tx.execute(insert, &[]).await.unwrap_err();
    assert_eq!(e.sqlstate(), Some("23505"));
    tx.rollback().await.unwrap();
}

/// A fresh transaction with the table, then one failing statement; returns its error.
async fn refused(h: &Harness, s: &ProjectionShape, amount: &str) -> erp_fields::Error {
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, s, "h27_measure", false, false).await;
    let e = tx
        .execute(
            "INSERT INTO h27_measure (account, label, day, posted, lines, amount) \
             VALUES (1, 'x', NULL, true, 1, $1::text::numeric)",
            &[&amount],
        )
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    e
}

#[tokio::test]
async fn out_of_range_numerics_fail_the_named_check_and_map_to_the_message() {
    let h = harness().await;
    let s = shape();
    for bad in ["NaN", "Infinity", "-Infinity", "1.005", "100000000.00"] {
        let e = refused(&h, &s, bad).await;
        assert_eq!(e.sqlstate(), Some("23514"), "{bad}");
        let name = e
            .constraint()
            .unwrap_or_else(|| panic!("no constraint name for {bad}"));
        let mapped = s.measure_for_constraint(name).unwrap();
        assert_eq!(mapped.kind(), ErrorKind::Validation);
        assert_eq!(
            mapped.message(),
            "H27 NumericOutOfRange: h27.probe: amount",
            "{bad}"
        );
    }

    // The predicate is FALSE, never NULL, for the special values and NULL.
    let mut tx = h.begin().await.unwrap();
    for operand in ["'NaN'", "'Infinity'", "'-Infinity'", "NULL"] {
        for i in 0..2 {
            let p = s.measure_range_predicate(i, operand).unwrap();
            let row = tx.query(&format!("SELECT {p}"), &[]).await.unwrap();
            assert_eq!(
                row[0].get::<_, Option<bool>>(0),
                Some(false),
                "{operand} {i}"
            );
        }
    }
    // int8 sums: sum(int8) is numeric, so the predicate runs before any int8 overflow.
    let int8 = s.measure_range_predicate(0, "sum(v.x)").unwrap();
    let over = tx
        .query(
            &format!("SELECT {int8} FROM (VALUES (9223372036854775807::int8), (1::int8)) v(x)"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(over[0].get::<_, Option<bool>>(0), Some(false));
    let under = tx
        .query(
            &format!("SELECT {int8} FROM (VALUES (-9223372036854775807::int8), (-2::int8)) v(x)"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(under[0].get::<_, Option<bool>>(0), Some(false));
    let fits = tx
        .query(
            &format!("SELECT {int8} FROM (VALUES (9223372036854775806::int8), (1::int8)) v(x)"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(fits[0].get::<_, Option<bool>>(0), Some(true));
    tx.rollback().await.unwrap();

    // A valid amount round-trips exactly, as admitted by our code.
    let admitted = s
        .admit_measures(vec![
            MeasureValue::Int8(7),
            MeasureValue::Numeric("12345678.90".parse().unwrap()),
        ])
        .unwrap();
    let MeasureValue::Numeric(exact) = s.measure_values(&admitted).unwrap()[1].clone() else {
        panic!("numeric expected")
    };
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s, "h27_measure", false, false).await;
    tx.execute(
        "INSERT INTO h27_measure (account, label, day, posted, lines, amount) \
         VALUES (1, 'x', NULL, true, 7, $1::text::numeric)",
        &[&exact.to_string()],
    )
    .await
    .unwrap();
    let back = tx
        .query("SELECT amount::text FROM h27_measure", &[])
        .await
        .unwrap();
    let text: String = back[0].get(0);
    assert_eq!(text.parse::<Decimal>().unwrap(), exact);
    assert_eq!(text, exact.to_string());
    tx.rollback().await.unwrap();
}
