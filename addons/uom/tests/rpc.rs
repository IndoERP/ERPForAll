//! Actual native RPC binder and model packaging contracts; HTTP transport gates are separate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::rpc::{BindError, Exposure, RpcArgs};
use erp_testkit::Harness;
#[path = "support/guard_fixture.rs"]
mod guard_fixture;

fn dec(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

fn text(value: &str) -> Cell {
    Cell::Str(Arc::from(value))
}

async fn harness() -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    h
}

async fn unit(tx: &mut Transaction, name: &str, parent: Option<i64>, relative: Decimal) -> Records {
    let units = tx.env().records("uom.uom").unwrap();
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("relative_factor", Cell::Decimal(relative));
    if let Some(parent) = parent {
        vals = vals.set_cell("relative_uom_id", Cell::Id(parent));
    }
    units.create(tx, vec![vals]).await.unwrap()
}

async fn convert(
    h: &Harness,
    tx: &mut Transaction,
    source: &Records,
    args: impl IntoIterator<Item = (&'static str, Value)>,
) -> Result<Value, BindError> {
    let exposure = h
        .orm()
        .exposure()
        .get(source.model_id(), "convert")
        .unwrap();
    let Exposure::Exposed(exposed) = exposure else {
        panic!("uom.uom.convert must be exposed through its actual RPC binder")
    };
    exposed
        .call(
            tx,
            source.clone(),
            RpcArgs::new(
                args.into_iter()
                    .map(|(key, value)| (key.to_owned(), value))
                    .collect(),
            ),
        )
        .await
}

#[tokio::test]
async fn rpc_defaults_to_up_and_names_current_precision() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    let score = unit(&mut tx, "Score", Some(units.ids()[0]), dec("20")).await;
    tx.env()
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Product Unit"))
                    .set_cell("digits", Cell::Int(0)),
            ],
        )
        .await
        .unwrap();
    let converted = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("2")),
            ("to_unit", Value::Int(score.ids()[0])),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        converted,
        Value::Map(Arc::new(
            [
                ("value".to_owned(), Value::Decimal(Decimal::ONE)),
                ("rounding".to_owned(), Value::str("UP")),
                ("digits".to_owned(), Value::Int(0)),
            ]
            .into_iter()
            .collect()
        ))
    );
    let exact = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("2")),
            ("to_unit", Value::Int(score.ids()[0])),
            ("exact", Value::Bool(true)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(exact, Value::Decimal(dec("0.1")));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_exact_returns_ratio_strings_for_nonterminating_results() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    let thirds = unit(&mut tx, "Triple", Some(units.ids()[0]), dec("3")).await;
    let exact = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("1")),
            ("to_unit", Value::Int(thirds.ids()[0])),
            ("exact", Value::Bool(true)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        exact,
        Value::Map(Arc::new(
            [
                ("numerator".to_owned(), Value::str("1")),
                ("denominator".to_owned(), Value::str("3")),
            ]
            .into_iter()
            .collect()
        ))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_rejects_malformed_decimal_and_unknown_rounding_without_fallback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    for (quantity, rounding, expected) in [
        (
            "1e3",
            "UP",
            "Invalid decimal amount: expected digits, optionally preceded by '-'.",
        ),
        (
            "0.12345678901234567890123456789",
            "UP",
            "Invalid decimal amount: more than 28 fractional digits.",
        ),
        (
            "1",
            "INVALID",
            "Unsupported unit of measure rounding method 'INVALID'.",
        ),
    ] {
        let error = convert(
            &h,
            &mut tx,
            &units,
            [
                ("quantity", Value::str(quantity)),
                ("to_unit", Value::Int(units.ids()[0])),
                ("rounding_method", Value::str(rounding)),
            ],
        )
        .await
        .unwrap_err();
        let BindError::Call(error) = error else {
            panic!("valid binder shapes must reach the model's explicit validation")
        };
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), expected);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn packaging_model_dispatch_preserves_identity_and_rounds_distinct_units() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    let six = unit(&mut tx, "Pack of six", Some(units.ids()[0]), dec("6")).await;
    let same = erp_sdk::__private::call_method::<(Decimal, i64, String), Decimal>(
        &mut tx,
        &units,
        "_check_qty",
        (dec("22.43"), units.ids()[0], "DOWN".to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(same, dec("22.43"));
    let multiple = erp_sdk::__private::call_method::<(Decimal, i64, String), Decimal>(
        &mut tx,
        &six,
        "_check_qty",
        (dec("22.43"), units.ids()[0], "DOWN".to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(multiple, dec("18"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_rejects_wrong_types_and_oversized_quantities() {
    use erp_sdk::rpc::ArgError;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    let error = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::Int(2)),
            ("to_unit", Value::Int(units.ids()[0])),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "argument 'quantity': expected a string, got integer"
    );
    let BindError::Argument {
        name,
        expected,
        error: ArgError::Mismatch { got },
    } = error
    else {
        panic!("integer quantity must fail as a binder type mismatch")
    };
    assert_eq!(name, "quantity");
    assert_eq!(expected, "a string");
    assert_eq!(got, "integer");
    let error = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("1".repeat(65))),
            ("to_unit", Value::Int(units.ids()[0])),
        ],
    )
    .await
    .unwrap_err();
    let BindError::Call(error) = error else {
        panic!("oversized text must reach bounded decimal validation")
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Invalid decimal amount: longer than 64 bytes."
    );
    let error = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("1")),
            ("to_unit", Value::Int(units.ids()[0])),
            ("rounding_method", Value::str("TOO-LONG-MODE")),
        ],
    )
    .await
    .unwrap_err();
    let BindError::Call(error) = error else {
        panic!("oversized rounding name must reach bounded option validation")
    };
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Unit of measure rounding method exceeds 9 bytes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_explicit_rounding_and_cross_root_zero_are_not_silent_fallbacks() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = unit(&mut tx, "Units", None, Decimal::ONE).await;
    let triple = unit(&mut tx, "Triple", Some(units.ids()[0]), dec("3")).await;
    let other = unit(&mut tx, "Length", None, Decimal::ONE).await;
    let rounded = convert(
        &h,
        &mut tx,
        &units,
        [
            ("quantity", Value::str("1")),
            ("to_unit", Value::Int(triple.ids()[0])),
            ("rounding_method", Value::str("DOWN")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        rounded,
        Value::Map(Arc::new(
            [
                ("value".to_owned(), Value::Decimal(dec("0.33"))),
                ("rounding".to_owned(), Value::str("DOWN")),
                ("digits".to_owned(), Value::Int(2)),
            ]
            .into_iter()
            .collect()
        ))
    );
    for exact in [false, true] {
        let error = convert(
            &h,
            &mut tx,
            &units,
            [
                ("quantity", Value::str("0")),
                ("to_unit", Value::Int(other.ids()[0])),
                ("exact", Value::Bool(exact)),
            ],
        )
        .await
        .unwrap_err();
        let BindError::Call(error) = error else {
            panic!("valid RPC shapes must return the typed cross-root error")
        };
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!(
                "Units of measure {} and {} do not share a reference root.",
                units.ids()[0],
                other.ids()[0]
            )
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
