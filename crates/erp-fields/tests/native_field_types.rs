//! Typed native field foundation; ORM/defaults/partition/ACL/WIT integration is separate.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::convert::{FieldRef, to_cell, value_to_json};
use erp_fields::rows::{Bitmap, Column, ColumnData, Key, Layout, RowsBuilder};
use erp_fields::{
    Cell, ColumnType, ErrorKind, FieldType, FieldValue, Storage, TimestampTz, Uuid, Value, Xid8,
};

fn field(name: &str) -> FieldRef<'_> {
    FieldRef {
        model: "native.fixture",
        field: name,
        label: name,
    }
}

#[test]
fn uuid_native_type_preserves_nil_and_null_and_refuses_char_substitution() {
    let id = Uuid::NIL;
    assert_eq!(FieldType::Uuid.column_type(), Some(ColumnType::Uuid));
    assert_eq!(FieldType::Uuid.storage(), Storage::Uuid);
    assert_eq!(ColumnType::Uuid.udt_name(), "uuid");
    let cell = to_cell(field("u"), &FieldType::Uuid, &Value::from(id.to_string())).unwrap();
    assert_eq!(cell, Cell::Uuid(id));
    assert_eq!(Option::<Uuid>::from_cell(&cell).unwrap(), Some(id));
    assert_eq!(Option::<Uuid>::from_cell(&Cell::Null).unwrap(), None);
    assert_eq!(Some(id).into_cell(), cell);
    assert!(cell.fits(Storage::Uuid));
    assert!(!cell.fits(Storage::Str));
    let value = Value::from_cell(&cell);
    assert_eq!(value, Value::Uuid(id));
    assert_ne!(value, Value::Str(id.to_string().into()));
    assert!(value.is_truthy());
    assert_eq!(
        value_to_json(&value).unwrap(),
        serde_json::Value::String(id.to_string())
    );
    assert_eq!(
        to_cell(field("u"), &FieldType::Uuid, &Value::False).unwrap(),
        Cell::Null
    );
    let error = to_cell(
        field("u"),
        &FieldType::Uuid,
        &Value::from("01234567-89AB-CDEF-0123-456789ABCDEF"),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "native.fixture.u: invalid UUID: expected canonical lowercase 8-4-4-4-12 hexadecimal text"
    );
}

#[test]
fn xid8_field_never_uses_signed_or_floating_number_input() {
    let id = Xid8::from_u64(u64::MAX);
    assert_eq!(FieldType::Xid8.column_type(), Some(ColumnType::Xid8));
    assert_eq!(FieldType::Xid8.storage(), Storage::Xid8);
    let cell = to_cell(field("x"), &FieldType::Xid8, &Value::from(id.to_string())).unwrap();
    assert_eq!(cell, Cell::Xid8(id));
    assert_eq!(Option::<Xid8>::from_cell(&cell).unwrap(), Some(id));
    assert_eq!(Option::<Xid8>::from_cell(&Cell::Null).unwrap(), None);
    assert_eq!(Value::from_cell(&cell), Value::Xid8(id));
    assert_eq!(
        value_to_json(&Value::from_cell(&cell)).unwrap(),
        serde_json::Value::String(id.to_string())
    );
    for wrong in [
        Value::Int(0),
        Value::Int(1),
        Value::Int(-1),
        Value::Float(1.0),
        Value::Bool(false),
    ] {
        let error = to_cell(field("x"), &FieldType::Xid8, &wrong).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "native.fixture.x: expected native xid8 or canonical unsigned decimal text"
        );
    }
    assert_eq!(
        to_cell(field("x"), &FieldType::Xid8, &Value::False).unwrap(),
        Cell::Null
    );
    assert!(Value::Xid8(Xid8::from_u64(0)).is_truthy());
}

#[test]
fn timezone_field_keeps_a_distinct_instant_type() {
    let instant = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
    assert_eq!(
        FieldType::TimestampTz.column_type(),
        Some(ColumnType::TimestampTz)
    );
    assert_eq!(FieldType::TimestampTz.storage(), Storage::TimestampTz);
    assert_eq!(ColumnType::TimestampTz.udt_name(), "timestamptz");
    let cell = to_cell(
        field("t"),
        &FieldType::TimestampTz,
        &Value::from(instant.to_string()),
    )
    .unwrap();
    assert_eq!(cell, Cell::TimestampTz(instant));
    assert_eq!(
        Option::<TimestampTz>::from_cell(&cell).unwrap(),
        Some(instant)
    );
    assert_eq!(Option::<TimestampTz>::from_cell(&Cell::Null).unwrap(), None);
    assert!(!cell.fits(Storage::DateTime));
    assert_eq!(Value::from_cell(&cell), Value::TimestampTz(instant));
    assert_eq!(
        value_to_json(&Value::from_cell(&cell)).unwrap(),
        serde_json::Value::String(instant.to_string())
    );
    let naive = Value::DateTime(erp_fields::DateTime::from_micros(0));
    let error = to_cell(field("t"), &FieldType::TimestampTz, &naive).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "native.fixture.t: expected native timestamptz or explicit-offset ISO text"
    );
}

#[test]
fn typed_native_rows_keep_values_distinct_from_null() {
    let layout = Layout::for_model(
        "native.fixture",
        [Key::from("u"), Key::from("x"), Key::from("t")],
    )
    .unwrap();
    let mut builder = RowsBuilder::new(layout, 2).unwrap();
    builder
        .column(
            "u",
            Column::new(ColumnData::Uuid(vec![Uuid::NIL; 2]))
                .with_false(Bitmap::from_rows([1]).unwrap()),
        )
        .unwrap();
    builder
        .column(
            "x",
            Column::new(ColumnData::Xid8(vec![Xid8::from_u64(0); 2]))
                .with_false(Bitmap::from_rows([1]).unwrap()),
        )
        .unwrap();
    builder
        .column(
            "t",
            Column::new(ColumnData::TimestampTz(vec![TimestampTz::UNIX_EPOCH; 2]))
                .with_false(Bitmap::from_rows([1]).unwrap()),
        )
        .unwrap();
    let rows = builder.finish().unwrap();
    assert_eq!(rows.get(0, "u").unwrap(), Value::Uuid(Uuid::NIL));
    assert_eq!(rows.get(0, "x").unwrap(), Value::Xid8(Xid8::from_u64(0)));
    assert_eq!(
        rows.get(0, "t").unwrap(),
        Value::TimestampTz(TimestampTz::UNIX_EPOCH)
    );
    for key in ["u", "x", "t"] {
        assert_eq!(rows.get(1, key).unwrap(), Value::False);
    }
}

#[test]
fn map_rows_retain_native_types_instead_of_inferring_char() {
    let one: erp_fields::ValueMap = [
        ("u".into(), Value::Uuid(Uuid::NIL)),
        ("x".into(), Value::Xid8(Xid8::from_u64(u64::MAX))),
        ("t".into(), Value::TimestampTz(TimestampTz::UNIX_EPOCH)),
    ]
    .into_iter()
    .collect();
    let absent: erp_fields::ValueMap = [
        ("u".into(), Value::False),
        ("x".into(), Value::False),
        ("t".into(), Value::False),
    ]
    .into_iter()
    .collect();
    let rows = erp_fields::rows::Rows::from_maps(vec![one, absent]).unwrap();
    assert!(matches!(
        rows.cell(0, 0).unwrap(),
        erp_fields::rows::CellRef::Uuid(_)
    ));
    assert!(matches!(
        rows.cell(0, 1).unwrap(),
        erp_fields::rows::CellRef::Xid8(_)
    ));
    assert!(matches!(
        rows.cell(0, 2).unwrap(),
        erp_fields::rows::CellRef::TimestampTz(_)
    ));
    for key in ["u", "x", "t"] {
        assert_eq!(rows.get(1, key).unwrap(), Value::False);
    }
}

#[cfg(feature = "pg")]
mod pg {
    use super::*;
    use bytes::BytesMut;
    use postgres_types::{FromSql, ToSql, Type};

    #[test]
    fn native_cells_bind_only_their_actual_sql_types() {
        let cases = [
            (
                FieldType::Uuid,
                "00000000-0000-0000-0000-000000000000",
                Type::UUID,
                "native uuid cell requires SQL type uuid",
            ),
            (
                FieldType::Xid8,
                "18446744073709551615",
                Type::XID8,
                "native xid8 cell requires SQL type xid8",
            ),
            (
                FieldType::TimestampTz,
                "1970-01-01T00:00:00Z",
                Type::TIMESTAMPTZ,
                "native timestamptz cell requires SQL type timestamptz",
            ),
        ];
        for (kind, text, ty, message) in cases {
            let cell = to_cell(field("wire"), &kind, &Value::from(text)).unwrap();
            let mut bytes = BytesMut::new();
            cell.to_sql_checked(&ty, &mut bytes).unwrap();
            match kind {
                FieldType::Uuid => assert_eq!(Uuid::from_sql(&ty, &bytes).unwrap(), Uuid::NIL),
                FieldType::Xid8 => assert_eq!(Xid8::from_sql(&ty, &bytes).unwrap().get(), u64::MAX),
                FieldType::TimestampTz => assert_eq!(
                    TimestampTz::from_sql(&ty, &bytes).unwrap(),
                    TimestampTz::UNIX_EPOCH
                ),
                _ => unreachable!("fixture only contains native types"),
            }
            let mut unchanged = BytesMut::from(&b"kept"[..]);
            let error = cell
                .to_sql_checked(&Type::TEXT, &mut unchanged)
                .err()
                .unwrap();
            let error = error.downcast_ref::<erp_fields::Error>().unwrap();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(error.message(), message);
            assert_eq!(&unchanged[..], b"kept");
        }
    }
}
