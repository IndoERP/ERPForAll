// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "pg")]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
use bytes::BytesMut;
use erp_fields::ErrorKind;
use erp_fields::money::{CanonicalFraction, CanonicalInteger, ExactNumericError, GridValue};
use postgres_types::{FromSql, IsNull, ToSql, Type};

fn frame(weight: i16, sign: u16, scale: u16, groups: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    out.extend_from_slice(&weight.to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&scale.to_be_bytes());
    for group in groups {
        out.extend_from_slice(&group.to_be_bytes());
    }
    out
}
fn invalid<T: std::fmt::Debug>(result: Result<T, Box<dyn std::error::Error + Send + Sync>>) {
    let error = result.unwrap_err();
    assert_eq!(
        error.downcast_ref::<ExactNumericError>(),
        Some(&ExactNumericError::InvalidEncoding)
    );
    assert_eq!(error.to_string(), "Invalid canonical exact cost encoding");
}

#[test]
fn wide_numeric_and_canonical_fraction_bind_without_decimal_narrowing() {
    for text in [
        "0",
        "-0.000000000000000001",
        "79228162514264337593543950336.000000000000000001",
    ] {
        let value = GridValue::parse(text, 18).unwrap();
        let mut raw = BytesMut::new();
        value.to_sql_checked(&Type::NUMERIC, &mut raw).unwrap();
        let decoded = GridValue::from_sql(&Type::NUMERIC, &raw)
            .unwrap()
            .rescale(18)
            .unwrap();
        assert_eq!(decoded, value);
    }
    let f = CanonicalFraction::parse("-2", "3").unwrap();
    let mut decoded = Vec::new();
    for part in [f.numerator(), f.denominator()] {
        let mut raw = BytesMut::new();
        part.to_sql_checked(&Type::NUMERIC, &mut raw).unwrap();
        decoded.push(CanonicalInteger::from_sql(&Type::NUMERIC, &raw).unwrap());
    }
    assert_eq!(
        CanonicalFraction::new(decoded[0].clone(), decoded[1].clone()).unwrap(),
        f
    );
    let value = GridValue::parse("3950.123456", 6).unwrap();
    let mut raw = BytesMut::new();
    value.to_sql_checked(&Type::NUMERIC, &mut raw).unwrap();
    // Independent PostgreSQL wire fixture: weight 0; groups 3950, 1234, 5600.
    assert_eq!(&raw[..], &frame(0, 0, 6, &[3950, 1234, 5600]));
    assert!(matches!(
        None::<GridValue>
            .to_sql_checked(&Type::NUMERIC, &mut BytesMut::new())
            .unwrap(),
        IsNull::Yes
    ));
    let values = vec![Some(value), None];
    let mut raw = BytesMut::new();
    values
        .to_sql_checked(&Type::NUMERIC_ARRAY, &mut raw)
        .unwrap();
    assert_eq!(
        Vec::<Option<GridValue>>::from_sql(&Type::NUMERIC_ARRAY, &raw).unwrap(),
        values
    );
}

#[test]
fn decoder_refuses_malformed_special_and_unbounded_numeric_frames() {
    for raw in [
        vec![],
        vec![0; 7],
        frame(0, 0xc000, 0, &[]),
        frame(0, 0xd000, 0, &[]),
        frame(0, 0xf000, 0, &[]),
        frame(0, 0x2000, 0, &[1]),
        frame(0, 0x4000, 0, &[]),
        frame(0, 0, 0, &[10000]),
        frame(i16::MAX, 0, 0, &[1]),
        frame(i16::MIN, 0, 0, &[1]),
        frame(0, 0, 0xffff, &[1]),
        frame(-1, 0, 0, &[1]),
        frame(0, 0, 0, &[1, 0]),
    ] {
        invalid(GridValue::from_sql(&Type::NUMERIC, &raw));
    }
    let mut extra = frame(0, 0, 0, &[1]);
    extra.push(0);
    invalid(GridValue::from_sql(&Type::NUMERIC, &extra));
    invalid(CanonicalInteger::from_sql(
        &Type::NUMERIC,
        &frame(-1, 0, 4, &[1]),
    ));
    let error = GridValue::from_sql(&Type::TEXT, &[]).unwrap_err();
    let typed = error.downcast_ref::<erp_fields::Error>().unwrap();
    assert_eq!(typed.kind(), ErrorKind::Programming);
    assert_eq!(typed.message(), "wide numeric SQL decoder expected numeric");
    let value = GridValue::parse("1", 18).unwrap();
    let mut out = BytesMut::from(&b"kept"[..]);
    let error = value
        .to_sql(&Type::INT8, &mut out)
        .err()
        .expect("wrong OID must fail");
    let typed = error.downcast_ref::<erp_fields::Error>().unwrap();
    assert_eq!(typed.kind(), ErrorKind::Programming);
    assert_eq!(typed.message(), "wide numeric SQL encoder expected numeric");
    assert_eq!(&out[..], b"kept");
    // Stored trailing scale is not significant precision; neither path rounds.
    assert_eq!(
        GridValue::from_sql(&Type::NUMERIC, &frame(0, 0, 100, &[1]))
            .unwrap()
            .to_string(),
        "1"
    );
    assert_eq!(
        CanonicalInteger::from_sql(&Type::NUMERIC, &frame(0, 0, 1, &[1]))
            .unwrap()
            .to_string(),
        "1"
    );
}
