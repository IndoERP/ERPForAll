//! Byte identity of the raw-row read (perf-stream §9.2): one statement whose rows
//! cover every column type the ORM reads (`read.rs` `decode` and `read_group/decode.rs` `raw`) and
//! their edge values, read once collected through the driver (`Row`) and once on the raw path
//! (`RawRow` over the driver's `DataRow` bodies). Every cell's bytes, the group decoder on every
//! cell and the cache decoder on every column type must agree, errors included (kind, message
//! and every source's text), prepared, unnamed and in a flight. A deliberately skewed range
//! mapping must be reported, so the check is shown to catch a wrong mapping.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use erp_fields::{ColumnType, RangeBounds};
use erp_orm::{IdentityMode, IdentityReport};
use erp_test_models::base_users;
use erp_testkit::Harness;

/// `(SQL type, one SQL value per row)`: row 0 all NULL, row 1 zero and empty values, row 2 the
/// largest values, row 3 the smallest and negative ones, row 4 the special values (NaN,
/// infinities, multi-dimensional arrays; some do not decode, on both paths alike), row 5 wide,
/// non-ASCII and nested values (a text value larger than the driver's 8 KiB read buffer).
const COLUMNS: [(&str, [&str; 6]); 30] = [
    ("bool", ["NULL", "false", "true", "false", "true", "true"]),
    (
        "int4",
        ["NULL", "0", "2147483647", "-2147483648", "1", "42"],
    ),
    (
        "int8",
        [
            "NULL",
            "0",
            "9223372036854775807",
            "-9223372036854775808",
            "1",
            "42",
        ],
    ),
    (
        "float8",
        [
            "NULL",
            "0",
            "1.7976931348623157e308",
            "'-0'",
            "'NaN'",
            "'-Infinity'",
        ],
    ),
    (
        "numeric",
        [
            "NULL",
            "0",
            "79228162514264337593543950335",
            "'-0.0000000000000000000000000001'",
            "'NaN'",
            "123456789012345678901234567890.5",
        ],
    ),
    (
        "varchar",
        ["NULL", "''", "'é'", "'x'", "'ab'", "'日本語😀'"],
    ),
    (
        "text",
        [
            "NULL",
            "''",
            "'😀'",
            "'a''b'",
            "'NULL'",
            "repeat('x', 20000)",
        ],
    ),
    (
        "date",
        [
            "NULL",
            "'2000-01-01'",
            "'5874897-12-31'",
            "'4713-01-01 BC'",
            "'infinity'",
            "'2026-10-20'",
        ],
    ),
    (
        "timestamp",
        [
            "NULL",
            "'2000-01-01 00:00:00'",
            "'294276-12-31 23:59:59.999999'",
            "'4713-01-01 00:00:00 BC'",
            "'-infinity'",
            "'2026-10-20 12:34:56.789012'",
        ],
    ),
    (
        "jsonb",
        [
            "NULL",
            "'{}'",
            r#"'{"a": [1, 2, {"b": null}], "c": 1.5e300}'"#,
            "'[]'",
            r#"'"é😀"'"#,
            "jsonb_build_object('k', repeat('y', 9000))",
        ],
    ),
    (
        "daterange",
        [
            "NULL",
            "'empty'",
            "'[2020-01-01,2020-02-01)'",
            "'(,2020-01-01]'",
            "'[2020-01-01,)'",
            "'(2020-01-01,2020-01-05]'",
        ],
    ),
    (
        "tsrange",
        [
            "NULL",
            "'empty'",
            "'[2020-01-01 10:00,2020-01-01 11:00)'",
            "'(,)'",
            "'[2020-01-01 10:00,infinity)'",
            "'(2020-01-01,2020-01-02]'",
        ],
    ),
    ("int2", ["NULL", "0", "32767", "-32768", "1", "7"]),
    (
        "float4",
        ["NULL", "0", "3.4028235e38", "'-0'", "'NaN'", "'Infinity'"],
    ),
    ("bpchar", ["NULL", "''", "'abc'", "'é'", "' '", "'x'"]),
    ("name", ["NULL", "''", "'pg_class'", "'é'", "'n'", "'x'"]),
    (
        "json",
        [
            "NULL",
            "'{}'",
            r#"'{"a": 1, "a": 2}'"#,
            "'[]'",
            "'null'",
            r#"'"x"'"#,
        ],
    ),
    (
        "bool[]",
        [
            "NULL",
            "'{}'",
            "'{t,f}'",
            "'{NULL}'",
            "'{{t,f},{f,t}}'",
            "'{t}'",
        ],
    ),
    (
        "int2[]",
        [
            "NULL",
            "'{}'",
            "'{32767,-32768}'",
            "'{NULL,1}'",
            "'{{1,2},{3,4}}'",
            "'{7}'",
        ],
    ),
    (
        "int4[]",
        [
            "NULL",
            "'{}'",
            "'{2147483647,-2147483648}'",
            "'{NULL,1}'",
            "'{{1,2},{3,4}}'",
            "'{7}'",
        ],
    ),
    (
        "int8[]",
        [
            "NULL",
            "'{}'",
            "'{9223372036854775807,-9223372036854775808}'",
            "'{NULL,1}'",
            "'{{1,2},{3,4}}'",
            "'{7}'",
        ],
    ),
    (
        "float4[]",
        [
            "NULL",
            "'{}'",
            "'{3.4028235e38}'",
            "'{NULL,-0}'",
            "'{NaN,Infinity}'",
            "'{1.5}'",
        ],
    ),
    (
        "float8[]",
        [
            "NULL",
            "'{}'",
            "'{1.7976931348623157e308}'",
            "'{NULL,-0}'",
            "'{NaN,-Infinity}'",
            "'{1.5}'",
        ],
    ),
    (
        "numeric[]",
        [
            "NULL",
            "'{}'",
            "'{79228162514264337593543950335}'",
            "'{NULL,-1.5}'",
            "'{NaN}'",
            "'{0.1}'",
        ],
    ),
    (
        "text[]",
        [
            "NULL",
            "'{}'",
            r#"'{"",NULL,"é😀"}'"#,
            "'{NULL}'",
            "'{{a,b},{c,d}}'",
            "ARRAY[repeat('z', 9000)]",
        ],
    ),
    (
        "varchar[]",
        [
            "NULL",
            "'{}'",
            "'{é,x}'",
            "'{NULL}'",
            "'{{a,b},{c,d}}'",
            "'{\"a b\"}'",
        ],
    ),
    (
        "bpchar[]",
        [
            "NULL",
            "'{}'",
            "'{abc,\"\"}'",
            "'{NULL}'",
            "'{{a,b},{c,d}}'",
            "'{x}'",
        ],
    ),
    (
        "date[]",
        [
            "NULL",
            "'{}'",
            "'{2000-01-01,5874897-12-31}'",
            "'{NULL}'",
            "'{infinity}'",
            "'{2026-10-20}'",
        ],
    ),
    (
        "timestamp[]",
        [
            "NULL",
            "'{}'",
            "'{\"2000-01-01 00:00:00\"}'",
            "'{NULL}'",
            "'{-infinity}'",
            "'{\"2026-10-20 12:34:56.789012\"}'",
        ],
    ),
    (
        "jsonb[]",
        [
            "NULL",
            "'{}'",
            "ARRAY['{\"a\": 1}'::jsonb]",
            "ARRAY[NULL::jsonb]",
            "ARRAY['[]'::jsonb]",
            "ARRAY['\"é\"'::jsonb]",
        ],
    ),
];

/// The statement: one row per value set, every value cast to its column's type.
fn statement() -> String {
    let rows: Vec<String> = (0..6)
        .map(|r| {
            let values: Vec<String> = COLUMNS
                .iter()
                .map(|(ty, values)| format!("({})::{ty}", values[r]))
                .collect();
            format!("({})", values.join(", "))
        })
        .collect();
    let names: Vec<String> = (0..COLUMNS.len()).map(|i| format!("c{i}")).collect();
    format!(
        "SELECT * FROM (VALUES {}) AS v({}) ",
        rows.join(", "),
        names.join(", ")
    )
}

/// The cache decoder's reads: each column type of `read.rs` on its own column (ranges with both
/// bound kinds), then reads the column does not support and a column past the last one, so the
/// errors are compared too.
fn reads() -> Vec<(usize, ColumnType)> {
    vec![
        (0, ColumnType::Bool),
        (1, ColumnType::Int4),
        (2, ColumnType::Int8),
        (3, ColumnType::Float8),
        (4, ColumnType::Numeric),
        (5, ColumnType::Varchar(None)),
        (5, ColumnType::Varchar(Some(10))),
        (6, ColumnType::Text),
        (7, ColumnType::Date),
        (8, ColumnType::Timestamp),
        (9, ColumnType::Jsonb),
        (10, ColumnType::DateRange(RangeBounds::HalfOpen)),
        (10, ColumnType::DateRange(RangeBounds::Closed)),
        (11, ColumnType::TsRange(RangeBounds::HalfOpen)),
        (11, ColumnType::TsRange(RangeBounds::Closed)),
        // wrong types: refused before decoding, on both paths
        (6, ColumnType::Int8),
        (2, ColumnType::Text),
        (9, ColumnType::Bool),
        (0, ColumnType::Int4),
        (12, ColumnType::Int4),
        // past the last column
        (COLUMNS.len(), ColumnType::Int8),
    ]
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn summary(mode: IdentityMode, report: &IdentityReport) -> String {
    format!(
        "{mode:?}: rows {}, cells {} ({} not NULL), decodes {}, errors on both paths {}, \
         mismatches {}",
        report.rows,
        report.cells,
        report.values,
        report.decodes,
        report.errors,
        report.mismatches.len()
    )
}

/// Both paths agree on every cell's bytes, every decode and every error, in each execution form.
#[tokio::test]
async fn raw_rows_decode_exactly_as_driver_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let sql = statement();
    let reads = reads();
    let width = COLUMNS.len();
    // (rows, columns) per statement: the flight's second statement leaves out the first row and
    // adds a trailing column
    for (mode, statements) in [
        (IdentityMode::Prepared, vec![(6, width)]),
        (IdentityMode::PerCall, vec![(6, width)]),
        (IdentityMode::Flight, vec![(6, width), (5, width + 1)]),
    ] {
        let report = tx
            .raw_row_identity_for_tests(&sql, mode, &reads, false)
            .await
            .unwrap();
        println!("{}", summary(mode, &report));
        assert!(
            report.mismatches.is_empty(),
            "{mode:?}:\n{}",
            report.mismatches.join("\n")
        );
        let rows: usize = statements.iter().map(|(r, _)| r).sum();
        let cells: usize = statements.iter().map(|(r, c)| r * c).sum();
        let decodes: usize = statements.iter().map(|(r, c)| r * (c + reads.len())).sum();
        assert_eq!(report.rows, rows, "{mode:?}");
        assert_eq!(report.cells, cells, "{mode:?}");
        assert_eq!(report.decodes, decodes, "{mode:?}");
        // per row at least the 5 wrong-type reads fail, plus the values that do not decode
        // (numeric NaN, infinite dates, multi-dimensional arrays)
        assert!(report.errors > 6 * rows, "{}", summary(mode, &report));
    }
    tx.rollback().await.unwrap();
}

/// A range mapping off by one byte (the test seam moves every value's range one byte on) is
/// reported: the check catches a wrong mapping.
#[tokio::test]
async fn a_skewed_range_mapping_is_reported() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let sql = statement();
    let reads = reads();
    for mode in [
        IdentityMode::Prepared,
        IdentityMode::PerCall,
        IdentityMode::Flight,
    ] {
        let report = tx
            .raw_row_identity_for_tests(&sql, mode, &reads, true)
            .await
            .unwrap();
        println!("skewed {}", summary(mode, &report));
        let bytes = report
            .mismatches
            .iter()
            .filter(|m| m.contains(" bytes: "))
            .count();
        // A value shifted one byte on reads other bytes (or falls outside its row) unless all its
        // bytes and the byte after it are equal (zeros before the next field's length prefix):
        // at least three in four non-NULL values must differ.
        assert!(
            report.values > 0 && bytes * 4 >= report.values * 3,
            "{mode:?}: {bytes} byte mismatches of {} values",
            report.values
        );
        assert!(
            report.mismatches.iter().any(|m| m.contains(" as Int8: ")),
            "{mode:?}: the cache decoder differs too"
        );
    }
    // the seam is off again: the same transaction reads identically
    let report = tx
        .raw_row_identity_for_tests(&sql, IdentityMode::Prepared, &reads, false)
        .await
        .unwrap();
    assert!(
        report.mismatches.is_empty(),
        "{}",
        report.mismatches.join("\n")
    );
    tx.rollback().await.unwrap();
}
