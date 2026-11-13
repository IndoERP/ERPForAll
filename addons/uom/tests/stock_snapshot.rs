// SPDX-License-Identifier: MIT OR Apache-2.0
//! Snapshot capture and the generated product-quantity column (database).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration assertions"
)]

use erp_addon_uom::conversion::Unit;
use erp_addon_uom::stock::{ProductUnitPreview, capture, product_quantity_sql};
use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn fixture() -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // This fixture exercises the installed SQL, but is not a module-lifecycle proof.
    tx.batch(include_str!("../src/guard_sql.sql"))
        .await
        .unwrap();
    tx.execute("INSERT INTO uom_reference_fence(singleton) VALUES (1)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn unit(tx: &mut Transaction, name: &str, parent: Option<i64>, factor: i64) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("relative_factor", Cell::Decimal(Decimal::from(factor)));
    if let Some(parent) = parent {
        vals = vals.set_cell("relative_uom_id", Cell::Id(parent));
    }
    tx.env()
        .records("uom.uom")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn marker_count(tx: &mut Transaction, ids: &[i64]) -> i64 {
    tx.query(
        "SELECT count(*) FROM uom_reference_use WHERE uom_id = ANY($1)",
        &[&ids.to_vec()],
    )
    .await
    .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn capture_registers_protects_reads_digits_and_refuses_different_roots() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Root", None, 1).await;
    let pack = unit(&mut tx, "Pack", Some(root), 6).await;
    let length = unit(&mut tx, "Length", None, 1).await;
    let weight = unit(&mut tx, "Weight", None, 1).await;
    tx.commit().await.unwrap();

    // Same root: both units are registered and the digits are the current setting.
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let snap = capture(&env, &mut tx, pack, root).await.unwrap();
    assert_eq!(
        (snap.line_unit(), snap.product_unit(), snap.root()),
        (pack, root, root)
    );
    assert_eq!(snap.source_factor(), Decimal::from(6));
    assert_eq!(snap.target_factor(), Decimal::ONE);
    let configured =
        erp_addon_base::decimal_precision::precision_get(&env, &mut tx, "Product Unit")
            .await
            .unwrap();
    assert_eq!(snap.digits(), configured);
    assert_eq!(marker_count(&mut tx, &[pack, root]).await, 2);
    // A changed setting is read by the next capture.
    let other: u32 = if configured == 5 { 4 } else { 5 };
    tx.env()
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Unit".into()))
                    .set_cell("digits", Cell::Int(other as i32)),
            ],
        )
        .await
        .unwrap();
    let again = capture(&env, &mut tx, pack, root).await.unwrap();
    assert_eq!(again.digits(), other);
    tx.commit().await.unwrap();

    // The registered units can no longer change their factors.
    let mut tx = h.begin().await.unwrap();
    for id in [pack, root] {
        let error = tx
            .savepoint(async |tx| {
                tx.env()
                    .records("uom.uom")?
                    .browse([id])?
                    .write(
                        tx,
                        RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(5))),
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

    // Different roots: refused before any write, also after the caller commits.
    let env = tx.env();
    let error = capture(&env, &mut tx, length, weight).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!("Units of measure {length} and {weight} do not share a reference root.")
    );
    assert_eq!(marker_count(&mut tx, &[length, weight]).await, 0);
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(marker_count(&mut tx, &[length, weight]).await, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Row {
    qty: String,
    source: String,
    target: String,
    digits: i32,
}

/// Rows where the exact ratio sits just below or above a HALF-UP tie that PostgreSQL's
/// division scale rounds onto the tie: `source/target` is `5 * 10^-(digits+1)` off by one in
/// the last place of `target`.
fn near_tie_rows() -> Vec<Row> {
    let mut rows = vec![];
    for d in 0..=6i32 {
        for a in [16u32, 18, 20] {
            let source = 5 * 10i128.pow(a);
            let power = 10i128.pow(a + d as u32 + 1);
            for target in [power + 1, power - 1] {
                for qty in ["1", "-1"] {
                    rows.push(Row {
                        qty: qty.to_owned(),
                        source: source.to_string(),
                        target: target.to_string(),
                        digits: d,
                    });
                }
            }
        }
    }
    rows
}

/// Create the generated column table, fill it, and return (differences from the service,
/// differences of the naive form, row count).
async fn compare(rows: &[Row]) -> (usize, i64, usize) {
    let qtys: Vec<String> = rows.iter().map(|r| r.qty.clone()).collect();
    let sources: Vec<String> = rows.iter().map(|r| r.source.clone()).collect();
    let targets: Vec<String> = rows.iter().map(|r| r.target.clone()).collect();
    let digits: Vec<i32> = rows.iter().map(|r| r.digits).collect();
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let expr = product_quantity_sql("qty", "source_factor", "target_factor", "digits").unwrap();
    tx.batch(&format!(
        "CREATE TABLE stock_qty_fixture(id serial PRIMARY KEY, qty numeric NOT NULL, \
         source_factor numeric NOT NULL, target_factor numeric NOT NULL, digits int NOT NULL, \
         product_qty numeric GENERATED ALWAYS AS ({expr}) STORED)"
    ))
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO stock_qty_fixture(qty, source_factor, target_factor, digits) \
         SELECT * FROM unnest($1::text[]::numeric[], $2::text[]::numeric[], $3::text[]::numeric[], $4::int[])",
        &[&qtys, &sources, &targets, &digits],
    )
    .await
    .unwrap();
    let stored_rows = tx
        .query(
            "SELECT qty::text, source_factor::text, target_factor::text, digits, product_qty::text \
             FROM stock_qty_fixture ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(stored_rows.len(), rows.len());
    let mut differences = 0usize;
    for row in &stored_rows {
        let qty = Decimal::from_str_exact(&row.get::<_, String>(0)).unwrap();
        let source = Decimal::from_str_exact(&row.get::<_, String>(1)).unwrap();
        let target = Decimal::from_str_exact(&row.get::<_, String>(2)).unwrap();
        let d: i32 = row.get(3);
        let stored = Decimal::from_str_exact(&row.get::<_, String>(4)).unwrap();
        let preview = ProductUnitPreview::new(
            &Unit::new(1, 1, source).unwrap(),
            &Unit::new(2, 1, target).unwrap(),
            d as u32,
        )
        .unwrap();
        if preview.product_quantity(qty).unwrap() != stored {
            differences += 1;
        }
    }
    let naive: i64 = tx
        .query(
            "SELECT count(*) FROM stock_qty_fixture \
             WHERE round(qty * source_factor / target_factor, digits) <> product_qty",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
    (differences, naive, rows.len())
}

/// Builder refusal (kind and full message) and a generated column over keyword-named columns,
/// run in PostgreSQL, so the quoting is proven where it matters.
async fn builder_refuses_and_quotes_keywords() {
    let error = product_quantity_sql("qty;x", "s", "t", "d").unwrap_err();
    assert_eq!(
        error.to_string(),
        "'qty;x' is not a lowercase SQL identifier (letters, digits and underscores, at most 63 bytes, not starting with a digit)."
    );
    assert_eq!(
        erp_sdk::Error::from(error).kind(),
        erp_sdk::ErrorKind::Programming
    );
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let expr = product_quantity_sql("user", "current_date", "target_factor", "digits").unwrap();
    tx.batch(&format!(
        "CREATE TABLE stock_qty_keywords(\"user\" numeric NOT NULL, \"current_date\" numeric NOT NULL, \
         target_factor numeric NOT NULL, digits int NOT NULL, \
         product_qty numeric GENERATED ALWAYS AS ({expr}) STORED)"
    ))
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO stock_qty_keywords VALUES ('-2.5', '1', '1', 0), ('1', '1', '3', 2)",
        &[],
    )
    .await
    .unwrap();
    let stored: Vec<String> = tx
        .query(
            "SELECT product_qty::text FROM stock_qty_keywords ORDER BY \"user\"",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    let expected: Vec<String> = [("-2.5", "1", 0u32), ("1", "3", 2)]
        .iter()
        .map(|(qty, target, d)| {
            ProductUnitPreview::new(
                &Unit::new(1, 1, Decimal::ONE).unwrap(),
                &Unit::new(2, 1, Decimal::from_str_exact(target).unwrap()).unwrap(),
                *d,
            )
            .unwrap()
            .product_quantity(Decimal::from_str_exact(qty).unwrap())
            .unwrap()
            .to_string()
        })
        .collect();
    assert_eq!(stored, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_column_equals_the_service_on_ten_thousand_rows() {
    builder_refuses_and_quotes_keywords().await;
    const ROWS: usize = 10_000;
    // (source, target) factor pairs: terminating, non-terminating and tie-prone ratios.
    let pairs: [(&str, &str); 18] = [
        ("1", "12"),
        ("7", "3"),
        ("1", "3"),
        ("12", "1"),
        ("5", "7"),
        ("1", "2"),
        ("1", "8"),
        ("3", "2"),
        ("5", "4"),
        ("0.001", "1"),
        ("1", "1000"),
        ("24", "6"),
        ("1", "1"),
        ("0.3", "0.07"),
        ("-1", "12"),
        ("7", "-3"),
        ("-5", "-7"),
        ("-1", "2"),
    ];
    // Digits 0..6 as before, plus the wide ones the Product Unit setting allows up to 28.
    let digit_choices = [0u32, 1, 2, 3, 4, 5, 6, 12, 18, 28];
    let mut rng = Lcg(0x5EED_CAFE);
    let mut rows = vec![];
    for i in 0..ROWS {
        let d = digit_choices[rng.below(digit_choices.len() as u64) as usize];
        let (s, t) = pairs[rng.below(pairs.len() as u64) as usize];
        // At 28 digits the rounded value must still fit a Decimal coefficient, so the
        // quantities there are below 0.1.
        let wide = d == 28;
        let mut mantissa = rng.below(if wide { 100 } else { 2_000_000_000 }) as i64;
        if i % 5 == 0 && !wide {
            // Near a tie: an odd mantissa at one digit finer than the step (an exact half step
            // when it ends in 5 and the ratio is 1), then sometimes one unit off.
            mantissa = (mantissa / 2) * 2 + 1;
            mantissa += [-1, 0, 0, 1][rng.below(4) as usize];
        }
        if rng.below(2) == 0 {
            mantissa = -mantissa;
        }
        let scale = if wide {
            3
        } else if i % 5 == 0 {
            d + 1
        } else {
            rng.below(7) as u32
        };
        rows.push(Row {
            qty: Decimal::new(mantissa, scale).to_string(),
            source: s.to_owned(),
            target: t.to_owned(),
            digits: d as i32,
        });
    }
    rows.extend(near_tie_rows());
    let (differences, naive, total) = compare(&rows).await;
    assert_eq!(differences, 0, "generated column differs from the service");
    eprintln!(
        "naive round(qty*source/target, digits) differs from the generated column on {naive} of {total} rows (random, wide-digit, negative-factor and near-tie rows)"
    );
}

#[tokio::test]
async fn exact_form_survives_division_scale_double_rounding() {
    let rows = near_tie_rows();
    let (differences, naive, total) = compare(&rows).await;
    assert_eq!(differences, 0, "generated column differs from the service");
    eprintln!("near-tie set: naive form differs on {naive} of {total} rows");
    assert!(
        naive >= 1,
        "the naive form never differed on the near-tie rows"
    );
}
