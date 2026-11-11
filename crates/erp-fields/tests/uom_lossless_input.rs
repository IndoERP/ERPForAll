//! Generic decimal admission regressions for the accepted lossless boundary.
//!
//! Dynamic ORM create/write converts values before addon overrides receive RawVals.
//! These converter tests do not establish persistence or whole-case UoM parity.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::convert::{DecimalPrecisionExceeded, FieldRef, to_cell};
use erp_fields::{Cell, Decimal, Digits, ErrorKind, FieldType, Value};

const FIELD: FieldRef<'static> = FieldRef {
    model: "test_orm.mixed",
    field: "numeric",
    label: "Numeric",
};

fn unrounded() -> FieldType {
    FieldType::Float {
        digits: Some(Digits::Unrounded),
    }
}

fn assert_precision_error(input: &str) {
    let error = to_cell(FIELD, &unrounded(), &Value::str(input))
        .expect_err("unrounded admission must reject precision loss");
    assert_eq!(error.kind(), ErrorKind::Validation);
    let source = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<DecimalPrecisionExceeded>()
        .unwrap();
    assert_eq!(source.field(), "test_orm.mixed.numeric");
    assert_eq!(source.input(), format!("'{}'", input));
    assert_eq!(source.max_digits(), 28);
    assert_eq!(
        error.message(),
        format!(
            "Decimal precision exceeded for test_orm.mixed.numeric: '{}' (maximum 28 digits)",
            input
        )
    );
}

#[test]
fn unrounded_rejects_decimal_digit_loss() {
    for input in [
        "0.12345678901234567890123456789",
        "-0.12345678901234567890123456789",
        "0.00000000000000000000000000001",
    ] {
        assert_precision_error(input);
    }
}

#[test]
fn unrounded_scientific_admission_is_exact() {
    for (input, expected) in [
        ("1.25e2", "125"),
        ("1.25E-2", "0.0125"),
        ("1e-28", "0.0000000000000000000000000001"),
        ("-1e28", "-10000000000000000000000000000"),
    ] {
        assert_eq!(
            to_cell(FIELD, &unrounded(), &Value::str(input)).unwrap(),
            Cell::Decimal(Decimal::from_str_exact(expected).unwrap()),
            "{input}"
        );
    }
    for input in [
        "1e-29",
        "1e29",
        "1.2345678901234567890123456789e0",
        "1.25e-28",
        "1e999999999999999999999999999999999",
    ] {
        assert_precision_error(input);
    }
}

#[test]
fn exact_decimal_magnitude_boundary_is_preserved() {
    let maximum = "79228162514264337593543950335";
    assert_eq!(
        to_cell(FIELD, &unrounded(), &Value::str(maximum)).unwrap(),
        Cell::Decimal(Decimal::MAX)
    );
    assert_precision_error("79228162514264337593543950336");
}

#[test]
fn malformed_decimal_keeps_generic_validation_error() {
    for input in ["", "abc", "1e", "1e+", "1e2e3", "1.2.3", "NaN", "Infinity"] {
        let value = Value::str(input);
        let error = to_cell(FIELD, &unrounded(), &value).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!("Invalid value for test_orm.mixed.numeric: {value}")
        );
    }
}

#[test]
fn fixed_digits_keep_intentional_half_up_rounding() {
    let field = FieldType::Float {
        digits: Some(Digits::Fixed {
            precision: 0,
            scale: 2,
        }),
    };
    for (input, expected) in [("2.675", "2.68"), ("-2.675", "-2.68"), ("2.675e0", "2.68")] {
        assert_eq!(
            to_cell(FIELD, &field, &Value::str(input)).unwrap(),
            Cell::Decimal(Decimal::from_str_exact(expected).unwrap())
        );
    }
}

#[test]
fn monetary_keeps_exact_input_for_currency_boundary() {
    let field = FieldType::Monetary {
        currency_field: Some("currency_id".to_owned()),
    };
    for input in ["14.70126", "1.470126e1"] {
        assert_eq!(
            to_cell(FIELD, &field, &Value::str(input)).unwrap(),
            Cell::Decimal(Decimal::from_str_exact("14.70126").unwrap())
        );
    }
    // Out-of-capacity inputs are rejected before the existing currency boundary;
    // the shared converter must never silently manufacture a rounded mantissa.
    let value = Value::str("0.12345678901234567890123456789");
    let error = to_cell(FIELD, &field, &value).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!("Invalid value for test_orm.mixed.numeric: {value}")
    );
}

#[test]
fn decimal_native_input_work_is_bounded() {
    let input = "0".repeat(4097);
    for field_type in [
        unrounded(),
        FieldType::Float {
            digits: Some(Digits::Fixed {
                precision: 0,
                scale: 2,
            }),
        },
        FieldType::Monetary {
            currency_field: None,
        },
    ] {
        let error = to_cell(FIELD, &field_type, &Value::str(&input)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        if matches!(
            field_type,
            FieldType::Float {
                digits: Some(Digits::Unrounded)
            }
        ) {
            assert_eq!(
                error.message(),
                "Decimal precision exceeded for test_orm.mixed.numeric: '<decimal input exceeds 4096 bytes>' (maximum 28 digits)"
            );
            let source = std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<DecimalPrecisionExceeded>()
                .unwrap();
            assert_eq!(source.input(), "'<decimal input exceeds 4096 bytes>'");
        } else {
            assert_eq!(
                error.message(),
                "Invalid value for test_orm.mixed.numeric: '<decimal input exceeds 4096 bytes>'"
            );
        }
    }
    assert_eq!(
        to_cell(FIELD, &unrounded(), &Value::str("0".repeat(4096))).unwrap(),
        Cell::Decimal(Decimal::ZERO)
    );
}

#[test]
fn scientific_scale_is_checked_after_exact_exponent_and_zero_reduction() {
    for input in [
        "1.0e-28",
        "+1.0E-28",
        "0.00000000000000000000000000001e1",
        "+0.00000000000000000000000000001E+1",
    ] {
        for (signed, expected) in [
            (input.to_owned(), "0.0000000000000000000000000001"),
            (
                format!("-{}", input.trim_start_matches('+')),
                "-0.0000000000000000000000000001",
            ),
        ] {
            for field_type in [
                unrounded(),
                FieldType::Float {
                    digits: Some(Digits::Fixed {
                        precision: 0,
                        scale: 28,
                    }),
                },
                FieldType::Monetary {
                    currency_field: None,
                },
            ] {
                assert_eq!(
                    to_cell(FIELD, &field_type, &Value::str(&signed)).unwrap(),
                    Cell::Decimal(Decimal::from_str_exact(expected).unwrap()),
                    "{signed} {field_type:?}"
                );
            }
        }
    }
}
