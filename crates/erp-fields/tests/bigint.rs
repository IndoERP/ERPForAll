// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use erp_fields::convert::{FieldRef, to_cell, value_to_json};
use erp_fields::{BigInt, Cell, ColumnType, ErrorKind, FieldType, FieldValue, Storage, Value};

fn field() -> FieldRef<'static> {
    FieldRef {
        model: "test.bigint",
        field: "counter",
        label: "Counter",
    }
}

#[test]
fn bigint_scalar_is_exact_signed_and_zero_is_not_null() {
    assert_eq!(FieldType::BigInt.column_type(), Some(ColumnType::BigInt));
    assert_eq!(FieldType::BigInt.storage(), Storage::BigInt);
    for n in [
        i64::MIN,
        -2147483649,
        -1,
        0,
        1,
        2147483648,
        9007199254740993,
        i64::MAX,
    ] {
        let v = BigInt::from_i64(n);
        assert_eq!(BigInt::parse(&n.to_string()).unwrap(), v);
        assert_eq!(v.get(), n);
        assert_eq!(v.into_cell(), Cell::BigInt(v));
        assert_eq!(BigInt::from_cell(&v.into_cell()).unwrap(), v);
        assert_eq!(
            Option::<BigInt>::from_cell(&v.into_cell()).unwrap(),
            Some(v)
        );
        let cell = to_cell(field(), &FieldType::BigInt, &Value::str(n.to_string())).unwrap();
        assert_eq!(cell, Cell::BigInt(v));
        assert_eq!(Value::from_cell(&cell), Value::BigInt(v));
        assert_eq!(
            value_to_json(&Value::BigInt(v)).unwrap(),
            serde_json::json!(n.to_string())
        );
    }
    assert_eq!(
        to_cell(field(), &FieldType::BigInt, &Value::Bool(false)).unwrap(),
        BigInt::from_i64(0).into_cell()
    );
    assert_eq!(None::<BigInt>.into_cell(), Cell::Null);
    assert_eq!(Option::<BigInt>::from_cell(&Cell::Null).unwrap(), None);
    assert_eq!(BigInt::from_cell(&Cell::Null).unwrap().get(), 0);
    assert_eq!(
        to_cell(field(), &FieldType::BigInt, &Value::False).unwrap(),
        BigInt::from_i64(0).into_cell()
    );
}

#[test]
fn bigint_input_refuses_noncanonical_overflow_and_unsafe_numbers() {
    for s in [
        "",
        "+1",
        "01",
        "-0",
        " 1",
        "1 ",
        "1.0",
        "1e2",
        "9223372036854775808",
        "-9223372036854775809",
        "١",
    ] {
        let e = BigInt::parse(s).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(
            e.message(),
            "invalid bigint: expected canonical signed 64-bit decimal text"
        );
    }
    for value in [
        Value::Int(9007199254740992),
        Value::Int(-9007199254740992),
        Value::Float(1.0),
        Value::Float(f64::NAN),
        Value::Bool(true),
        Value::str("01"),
    ] {
        let e = to_cell(field(), &FieldType::BigInt, &value).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(
            e.message(),
            "test.bigint.counter: expected native bigint, a safe integer or canonical signed 64-bit decimal text"
        );
    }
    for n in [-9007199254740991, 0, 9007199254740991] {
        assert_eq!(
            to_cell(field(), &FieldType::BigInt, &Value::Int(n)).unwrap(),
            Cell::BigInt(BigInt::from_i64(n))
        );
    }
}
