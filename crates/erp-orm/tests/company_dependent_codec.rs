// SPDX-License-Identifier: MIT OR Apache-2.0
//! Storage codec guarantees; these do not claim complete ORM spec cases.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{Cell, Decimal, Digits, ErrorKind, FieldType};
use erp_orm::{CompanyId, company_dependent as cd};
use serde_json::json;
use std::sync::Arc;

fn raw(v: serde_json::Value) -> Cell {
    Cell::Json(Arc::new(v))
}

#[test]
fn exact_company_decimal_never_round_trips_through_float() {
    let ty = FieldType::Float {
        digits: Some(Digits::Fixed {
            precision: 28,
            scale: 6,
        }),
    };
    let value = Cell::Decimal("9007199254740993.123456".parse::<Decimal>().unwrap());
    let encoded = cd::encode(&ty, &value).unwrap();
    assert_eq!(encoded, json!("9007199254740993.123456"));
    assert_eq!(cd::decode(&ty, &encoded).unwrap(), value);
    let err = cd::decode(&ty, &json!(1.25)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("company-dependent"));
}

#[test]
fn company_map_distinguishes_null_from_missing_and_removes_equal_defaults() {
    let ty = FieldType::Text;
    let company = CompanyId::new(1).unwrap();
    let fallback = Cell::Str("default".into());
    let original = raw(json!({"2": "other"}));
    assert_eq!(
        cd::project(&ty, &original, company, &fallback).unwrap(),
        fallback
    );
    let explicit = cd::apply(&ty, &original, company, &Cell::Null, &fallback).unwrap();
    assert_eq!(
        cd::project(&ty, &explicit, company, &fallback).unwrap(),
        Cell::Null
    );
    assert_eq!(explicit, raw(json!({"1": null, "2": "other"})));
    let inherited = cd::apply(&ty, &explicit, company, &fallback, &fallback).unwrap();
    assert_eq!(inherited, original);
    assert_eq!(
        cd::delta(&explicit, &inherited).unwrap(),
        raw(json!({"1": []}))
    );
    assert_eq!(
        cd::delta(&original, &explicit).unwrap(),
        raw(json!({"1": [null]}))
    );
}

#[test]
fn company_zero_and_false_match_absent_defaults() {
    for (ty, value) in [
        (FieldType::Boolean, Cell::Bool(false)),
        (FieldType::Integer, Cell::Int(0)),
        (FieldType::Float { digits: None }, Cell::Float(0.0)),
    ] {
        assert_eq!(
            cd::apply(
                &ty,
                &Cell::Null,
                CompanyId::new(1).unwrap(),
                &value,
                &Cell::Null
            )
            .unwrap(),
            Cell::Null
        );
    }
}

#[test]
fn company_codec_refuses_malformed_keys_shapes_and_nonfinite_values() {
    for value in [
        json!([]),
        json!({"0": 1}),
        json!({"01": 1}),
        json!({"-1": 1}),
        json!({"1": []}),
        json!({"1": "bad"}),
    ] {
        let err = cd::project(
            &FieldType::Integer,
            &raw(value),
            CompanyId::new(1).unwrap(),
            &Cell::Null,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(err.message().contains("company-dependent"));
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = cd::encode(&FieldType::Float { digits: None }, &Cell::Float(value)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert!(err.message().contains("finite"));
    }
}

#[test]
fn company_codec_enforces_bounds_and_compares_decimal_values() {
    let huge = Cell::Str("x".repeat(cd::MAX_MAP_BYTES + 1).into());
    let err = cd::encode(&FieldType::Text, &huge).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().contains("byte limit"));
    let ty = FieldType::Float {
        digits: Some(Digits::Unrounded),
    };
    assert_eq!(
        cd::apply(
            &ty,
            &Cell::Null,
            CompanyId::new(1).unwrap(),
            &Cell::Decimal("0.000".parse().unwrap()),
            &Cell::Null
        )
        .unwrap(),
        Cell::Null
    );
    let err = cd::decode(&FieldType::Integer, &json!(i64::MAX)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("company-dependent integer"));
}

#[test]
fn company_codec_checks_direct_decode_bounds_before_copying() {
    let value = json!("x".repeat(cd::MAX_MAP_BYTES + 1));
    let result = cd::decode(&FieldType::Text, &value);
    assert!(result.is_err(), "oversized stored strings must be rejected");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().contains("byte limit"));
}

#[test]
fn company_temporal_storage_is_canonical_and_preserves_microseconds() {
    use erp_fields::{Date, DateTime};
    let date = Date::from_ymd(2026, 11, 11).unwrap();
    for value in [
        Cell::Date(date),
        Cell::DateTime(DateTime::from_parts(date, 1, 2, 3, 123456).unwrap()),
    ] {
        let ty = if matches!(value, Cell::Date(_)) {
            FieldType::Date
        } else {
            FieldType::Datetime
        };
        let encoded = cd::encode(&ty, &value).unwrap();
        assert_eq!(cd::decode(&ty, &encoded).unwrap(), value);
    }
    for (ty, value) in [
        (FieldType::Date, "2026-11-11 trailing"),
        (FieldType::Datetime, "2026"),
    ] {
        let err = cd::decode(&ty, &json!(value)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(err.message().contains("company-dependent"));
    }
}

#[test]
fn company_scalar_kinds_roundtrip_without_coercion() {
    use erp_fields::OnDelete;
    for (ty, value) in [
        (FieldType::Char { size: None }, Cell::Str("élang".into())),
        (FieldType::Text, Cell::Str("".into())),
        (FieldType::Html, Cell::Str("<p>ok</p>".into())),
        (FieldType::Boolean, Cell::Bool(true)),
        (FieldType::Integer, Cell::Int(i32::MIN)),
        (FieldType::Float { digits: None }, Cell::Float(-1.25)),
        (
            FieldType::Monetary {
                currency_field: None,
            },
            Cell::Decimal("-9007199254740993.25".parse().unwrap()),
        ),
        (
            FieldType::Selection {
                values: None,
                method: Some("keys".into()),
            },
            Cell::Str("active".into()),
        ),
        (
            FieldType::Many2one {
                comodel: "res.partner".into(),
                ondelete: OnDelete::SetNull,
            },
            Cell::Id(i64::MAX),
        ),
    ] {
        assert!(cd::supports(&ty));
        assert_eq!(
            cd::decode(&ty, &cd::encode(&ty, &value).unwrap()).unwrap(),
            value
        );
    }
    for ty in [FieldType::Json, FieldType::Id, FieldType::Uuid] {
        assert!(!cd::supports(&ty));
        let err = cd::encode(&ty, &Cell::Null).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(err.message().contains("company-dependent"));
    }
}

#[test]
fn company_maps_bound_entries_and_combined_payload_and_redact_errors() {
    let entries: serde_json::Map<String, serde_json::Value> = (1..=cd::MAX_COMPANIES + 1)
        .map(|id| (id.to_string(), json!(null)))
        .collect();
    for (map, phrase) in [
        (serde_json::Value::Object(entries), "company limit"),
        (json!({"1":"x".repeat(cd::MAX_MAP_BYTES)}), "byte limit"),
    ] {
        let result = cd::project(
            &FieldType::Text,
            &raw(map),
            CompanyId::new(1).unwrap(),
            &Cell::Null,
        );
        assert!(result.is_err(), "oversized map must refuse");
        let err = result.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert!(err.message().contains(phrase));
    }
    let err = cd::project(
        &FieldType::Integer,
        &raw(json!({"2":"hidden-secret-value"})),
        CompanyId::new(1).unwrap(),
        &Cell::Null,
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("company-dependent integer"));
    assert!(!err.message().contains("hidden-secret-value"));
}
