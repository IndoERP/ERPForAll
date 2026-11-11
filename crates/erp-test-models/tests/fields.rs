//! FIELD cases (S1 scope: values, storage and schema; search SQL is S2).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Arc;

use erp_sdk::{Base, Cell, Date, DateTime, Decimal, ErrorKind, RecId, Vals, Value};
use erp_test_models::test_orm::{Mixed, Moment, Partner, Schema, Selection, WriteOverride};
use erp_testkit::assert_query_count;
use support::{map, one, rows};

fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime {
    DateTime::from_parts(Date::from_ymd(y, m, d).unwrap(), h, mi, s, 0).unwrap()
}

#[tokio::test]
async fn spec_field_009_reading_a_field_on_several_records() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let rs = partners
        .create(
            &mut tx,
            vec![
                Vals::new().set(Partner::NAME, "a"),
                Vals::new().set(Partner::NAME, "b"),
            ],
        )
        .await
        .unwrap();
    let err = rs.get(&mut tx, Partner::NAME).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let ids = rs.ids();
    assert_eq!(
        err.message(),
        format!(
            "Expected singleton: test_orm.partner({}, {})",
            ids[0], ids[1]
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_010_null_value_on_an_empty_recordset() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let empty = tx.env().model::<Mixed>().unwrap();
    let values = assert_query_count!(tx, <= 0, {
        let mut out = Vec::new();
        for f in [
            "boolean", "integer", "float", "number", "monetary", "char", "text", "html", "selection",
            "date", "moment", "json", "id",
        ] {
            out.push((f, empty.value(&mut tx, f).await.unwrap()));
        }
        out
    });
    let expected = [
        ("boolean", Value::Bool(false)),
        ("integer", Value::Int(0)),
        ("float", Value::Float(0.0)),
        ("number", Value::Float(0.0)),
        ("monetary", Value::Float(0.0)),
        ("char", Value::False),
        ("text", Value::False),
        ("html", Value::False),
        ("selection", Value::False),
        ("date", Value::False),
        ("moment", Value::False),
        ("json", Value::False),
        ("id", Value::False),
    ];
    assert_eq!(values, expected);
    // typed: null values of the Rust types
    assert!(!empty.get(&mut tx, Mixed::BOOLEAN).await.unwrap());
    assert_eq!(empty.get(&mut tx, Mixed::INTEGER).await.unwrap(), 0);
    assert_eq!(empty.get(&mut tx, Mixed::CHAR).await.unwrap(), None);
    assert_eq!(empty.get(&mut tx, Base::ID).await.unwrap(), None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_011_assignment_goes_through_write() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<WriteOverride>().unwrap();
    let recs = model
        .create(&mut tx, vec![Vals::new(), Vals::new()])
        .await
        .unwrap();
    let record = recs.at(0).unwrap();
    record
        .set(&mut tx, WriteOverride::PRIORITY, 4)
        .await
        .unwrap();
    assert_eq!(
        record.get(&mut tx, WriteOverride::PRIORITY).await.unwrap(),
        5
    );
    recs.set(&mut tx, WriteOverride::BODY, "Updated")
        .await
        .unwrap();
    let bodies = recs.mapped(&mut tx, WriteOverride::BODY).await.unwrap();
    assert_eq!(bodies, vec![Some(Arc::from("Updated")); 2]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_012_the_id_field() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let rs = partners
        .create(&mut tx, vec![Vals::new(), Vals::new()])
        .await
        .unwrap();
    let r = rs.at(0).unwrap();
    assert!(r.id().unwrap().is_some_and(|id| id.get() > 0));
    assert_eq!(partners.id().unwrap(), None);
    assert_eq!(rs.id().unwrap_err().kind(), ErrorKind::Programming);
    let err = r.set_value(&mut tx, "id", Value::Int(5)).await.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(err.message(), "field 'id' cannot be assigned");
    let err = r
        .set(&mut tx, Base::ID, RecId::from_real(5).unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(err.message(), "field 'id' cannot be assigned");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_014_reading_a_deleted_record() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let ghost = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .browse([999_999])
        .unwrap();
    let err = ghost.get(&mut tx, Partner::NAME).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        "Record does not exist or has been deleted.\n(Record: test_orm.partner(999999,), User: 1)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_015_writing_an_unchanged_value() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(Partner::NAME, "X")])
        .await
        .unwrap();
    rec.write(&mut tx, Vals::new().set(Partner::NAME, "X"))
        .await
        .unwrap();
    assert!(!tx.is_dirty(&rec, "name").unwrap());
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    let update = tx.captured().join("\n");
    assert!(
        update.starts_with("UPDATE \"test_orm_partner\""),
        "{update}"
    );
    assert!(update.contains("\"write_uid\"") && update.contains("\"write_date\""));
    assert!(!update.contains("\"name\""), "{update}");
    // not in cache: written
    tx.invalidate_records(&rec, Some(&["name"])).await.unwrap();
    rec.write(&mut tx, Vals::new().set(Partner::NAME, "X"))
        .await
        .unwrap();
    assert!(tx.is_dirty(&rec, "name").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_016_integer_null_semantics() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_orm.partner").unwrap();
    let recs = model
        .create_values(
            &mut tx,
            &[
                map(&[("number", Value::Int(42))]),
                map(&[("number", Value::Int(0))]),
                map(&[("number", Value::False)]),
                map(&[]),
            ],
        )
        .await
        .unwrap();
    let expected = vec![Value::Int(42), Value::Int(0), Value::Int(0), Value::Int(0)];
    assert_eq!(recs.values(&mut tx, "number").await.unwrap(), expected);
    tx.invalidate_all().await.unwrap();
    assert_eq!(recs.values(&mut tx, "number").await.unwrap(), expected);
    let stored = rows(
        &mut tx,
        "SELECT number::text FROM test_orm_partner ORDER BY id",
    )
    .await;
    assert_eq!(
        stored,
        vec![
            vec![Some("42".into())],
            vec![Some("0".into())],
            vec![Some("0".into())],
            vec![None]
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_017_char_empty_string_vs_null() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_orm.partner").unwrap();
    let recs = model
        .create_values(
            &mut tx,
            &[
                map(&[("name", Value::str("name"))]),
                map(&[("name", Value::str(""))]),
                map(&[("name", Value::False)]),
                map(&[]),
            ],
        )
        .await
        .unwrap();
    let expected = vec![
        Value::str("name"),
        Value::str(""),
        Value::False,
        Value::False,
    ];
    assert_eq!(recs.values(&mut tx, "name").await.unwrap(), expected);
    tx.invalidate_all().await.unwrap();
    assert_eq!(recs.values(&mut tx, "name").await.unwrap(), expected);
    let stored = rows(&mut tx, "SELECT name FROM test_orm_partner ORDER BY id").await;
    assert_eq!(
        stored,
        vec![
            vec![Some("name".into())],
            vec![Some(String::new())],
            vec![None],
            vec![None]
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_018_boolean_null_reads_false() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Mixed>().unwrap();
    let recs = model
        .create(
            &mut tx,
            vec![
                Vals::new().set(Mixed::BOOLEAN, true),
                Vals::new().set(Mixed::BOOLEAN, false),
                Vals::new(),
            ],
        )
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        recs.mapped(&mut tx, Mixed::BOOLEAN).await.unwrap(),
        [true, false, false]
    );
    let stored = rows(
        &mut tx,
        "SELECT boolean::text FROM test_orm_mixed ORDER BY id",
    )
    .await;
    assert_eq!(
        stored,
        vec![
            vec![Some("true".into())],
            vec![Some("false".into())],
            vec![None]
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_019_input_coercion() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_orm.mixed").unwrap();
    let rec = model.create_values(&mut tx, &[map(&[])]).await.unwrap();
    let ok = [
        ("integer", Value::str("12"), Value::Int(12)),
        ("float", Value::str("1.5"), Value::Float(1.5)),
        ("boolean", Value::Int(0), Value::Bool(false)),
        ("char", Value::Int(42), Value::str("42")),
        (
            "char",
            Value::Bytes(Arc::from(&b"abc"[..])),
            Value::str("abc"),
        ),
    ];
    for (field, input, expected) in ok {
        rec.set_value(&mut tx, field, input).await.unwrap();
        assert_eq!(
            rec.value(&mut tx, field).await.unwrap(),
            expected,
            "{field}"
        );
    }
    for (field, input) in [
        ("integer", Value::Float(1.9)),
        ("integer", Value::str("abc")),
        ("boolean", Value::str("False")),
    ] {
        let err = rec
            .set_value(&mut tx, field, input.clone())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{field} {input}");
        assert!(
            err.message().contains(&format!("test_orm.mixed.{field}")),
            "{err}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_020_float_without_digits() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Mixed>().unwrap();
    let rec = model
        .create(&mut tx, vec![Vals::new().set(Mixed::FLOAT, 0.1 + 0.2)])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Mixed::FLOAT).await.unwrap(),
        0.300_000_000_000_000_04
    );
    let ty = rows(
        &mut tx,
        "SELECT data_type::text, numeric_precision::text FROM information_schema.columns \
         WHERE table_name = 'test_orm_mixed' AND column_name = 'float'",
    )
    .await;
    assert_eq!(
        ty,
        vec![vec![Some("double precision".into()), Some("53".into())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_021_float_with_digits() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_orm.mixed").unwrap();
    let rec = model.create_values(&mut tx, &[map(&[])]).await.unwrap();
    for (input, expected) in [
        (2.496_093_75, "2.50"),
        (1.1, "1.10"),
        (2.675, "2.68"),
        (-2.675, "-2.68"),
    ] {
        rec.set_value(&mut tx, "number", Value::Float(input))
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let expected: Decimal = expected.parse().unwrap();
        assert_eq!(
            rec.cell(&mut tx, "number").await.unwrap(),
            Cell::Decimal(expected)
        );
        let stored = one(&mut tx, "SELECT number::text FROM test_orm_mixed").await;
        assert_eq!(
            stored.unwrap().parse::<Decimal>().unwrap(),
            expected,
            "{input}"
        );
    }
    let ty = rows(
        &mut tx,
        "SELECT data_type::text, numeric_precision::text, numeric_scale::text \
         FROM information_schema.columns WHERE table_name = 'test_orm_mixed' AND column_name = 'number'",
    )
    .await;
    assert_eq!(ty, vec![vec![Some("numeric".into()), None, None]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_027_028_temporal_inputs() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_orm.mixed").unwrap();
    let rec = model.create_values(&mut tx, &[map(&[])]).await.unwrap();
    let d = Date::from_ymd(2012, 5, 1).unwrap();
    for input in [
        Value::Date(d),
        Value::str("2012-05-01"),
        Value::str("2012-05-01 10:45:00"),
        Value::DateTime(dt(2012, 5, 1, 10, 45, 0)),
    ] {
        rec.set_value(&mut tx, "date", input).await.unwrap();
        tx.invalidate_all().await.unwrap();
        assert_eq!(rec.value(&mut tx, "date").await.unwrap(), Value::Date(d));
    }
    rec.set_value(&mut tx, "date", Value::False).await.unwrap();
    assert_eq!(rec.value(&mut tx, "date").await.unwrap(), Value::False);
    let err = rec
        .set_value(&mut tx, "date", Value::str("12-5-1"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);

    for (input, expected) in [
        (
            Value::str("2012-05-01"),
            Value::DateTime(dt(2012, 5, 1, 0, 0, 0)),
        ),
        (
            Value::str("2012-05-01 06:00:00"),
            Value::DateTime(dt(2012, 5, 1, 6, 0, 0)),
        ),
        (Value::Date(d), Value::DateTime(dt(2012, 5, 1, 0, 0, 0))),
        (Value::False, Value::False),
    ] {
        rec.set_value(&mut tx, "moment", input).await.unwrap();
        tx.invalidate_all().await.unwrap();
        assert_eq!(rec.value(&mut tx, "moment").await.unwrap(), expected);
    }
    let err = rec
        .set_value(&mut tx, "moment", Value::str("12-5-1"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_029_datetime_utc_storage_and_timezone_display() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let moment = dt(2024, 1, 1, 20, 0, 0);
    let rec = tx
        .env()
        .model::<Moment>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(Moment::MOMENT, moment)])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Moment::MOMENT).await.unwrap(),
        Some(moment)
    );
    let jakarta = rec.with_context([("tz", Value::str("Asia/Jakarta"))]);
    assert_eq!(
        jakarta
            .get(&mut tx, Base::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("2024-01-02 03:00:00")
    );
    let mars = rec.with_context([("tz", Value::str("Mars/Base"))]);
    assert_eq!(
        mars.get(&mut tx, Base::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("2024-01-01 20:00:00")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_032_selection_value_validation() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test_orm.selection")
        .unwrap()
        .create_values(&mut tx, &[map(&[])])
        .await
        .unwrap();
    rec.set_value(&mut tx, "state", Value::str("foo"))
        .await
        .unwrap();
    assert_eq!(
        rec.value(&mut tx, "state").await.unwrap(),
        Value::str("foo")
    );
    rec.set_value(&mut tx, "state", Value::False).await.unwrap();
    assert_eq!(rec.value(&mut tx, "state").await.unwrap(), Value::False);
    let err = rec
        .set_value(&mut tx, "state", Value::str("zz_ZZ"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_orm.selection.state: 'zz_ZZ'"
    );
    // a method-backed selection is not validated
    let mixed = tx
        .env()
        .records("test_orm.mixed")
        .unwrap()
        .create_values(&mut tx, &[map(&[("selection_str", Value::str("zz_ZZ"))])])
        .await
        .unwrap();
    assert_eq!(
        mixed.value(&mut tx, "selection_str").await.unwrap(),
        Value::str("zz_ZZ")
    );
    let _ = Selection::STATE;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_037_json_round_trip() {
    use erp_sdk::serde_json::json;
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .model::<Mixed>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(Mixed::JSON, Some(json!({"delete_messages": []})))],
        )
        .await
        .unwrap();
    let mut first = rec.get(&mut tx, Mixed::JSON).await.unwrap().unwrap();
    let second = rec.get(&mut tx, Mixed::JSON).await.unwrap().unwrap();
    assert_eq!(first, second);
    first["delete_messages"] = json!(["mutated"]);
    assert_eq!(
        rec.get(&mut tx, Mixed::JSON).await.unwrap().unwrap(),
        second
    );
    rec.set(
        &mut tx,
        Mixed::JSON,
        Some(json!({"delete_messages": ["x"]})),
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Mixed::JSON).await.unwrap(),
        Some(json!({"delete_messages": ["x"]}))
    );
    rec.set_value(
        &mut tx,
        "json",
        Value::list([Value::str("x"), Value::str("y")]),
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Mixed::JSON).await.unwrap(),
        Some(json!(["x", "y"]))
    );
    rec.set(&mut tx, Mixed::JSON, Some(json!({})))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        one(&mut tx, "SELECT json::text FROM test_orm_mixed").await,
        None
    );
    assert_eq!(rec.value(&mut tx, "json").await.unwrap(), Value::False);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_038_char_size_column() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let cols = rows(
        &mut tx,
        "SELECT column_name::text, data_type::text, character_maximum_length::text \
         FROM information_schema.columns WHERE table_name = 'test_orm_schema' \
         AND column_name IN ('size', 'required') ORDER BY 1",
    )
    .await;
    assert_eq!(
        cols,
        vec![
            vec![
                Some("required".into()),
                Some("character varying".into()),
                None
            ],
            vec![
                Some("size".into()),
                Some("character varying".into()),
                Some("3".into())
            ],
        ]
    );
    let _ = Schema::SIZE;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_040_column_type_mapping() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let cols = rows(
        &mut tx,
        "SELECT c.column_name::text, c.udt_name::text, c.is_nullable::text, c.is_identity::text, \
                col_description('test_orm_mixed'::regclass, c.ordinal_position::int) \
         FROM information_schema.columns c WHERE c.table_name = 'test_orm_mixed' ORDER BY 1",
    )
    .await;
    let get = |name: &str| {
        cols.iter()
            .find(|r| r[0].as_deref() == Some(name))
            .cloned()
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    for (name, udt) in [
        ("integer", "int4"),
        ("float", "float8"),
        ("number", "numeric"),
        ("numeric", "numeric"),
        ("monetary", "numeric"),
        ("char", "varchar"),
        ("text", "text"),
        ("html", "text"),
        ("selection", "varchar"),
        ("date", "date"),
        ("moment", "timestamp"),
        ("boolean", "bool"),
        ("json", "jsonb"),
        ("currency_id", "int8"),
    ] {
        let row = get(name);
        assert_eq!(row[1].as_deref(), Some(udt), "{name}");
        assert_eq!(row[2].as_deref(), Some("YES"), "{name} nullable");
    }
    let id = get("id");
    assert_eq!(
        (id[1].as_deref(), id[2].as_deref(), id[3].as_deref()),
        (Some("int8"), Some("NO"), Some("YES"))
    );
    assert_eq!(get("char")[4].as_deref(), Some("Char"));
    assert_eq!(get("currency_id")[4].as_deref(), Some("Currency"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_077_field_description_attributes() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = tx.env().records("test_orm.mixed").unwrap();
    let fields = mixed.fields_get(&mut tx, None, None).await.unwrap();

    let b = &fields["boolean"];
    assert_eq!(b["type"], Value::str("boolean"));
    assert_eq!(b["store"], Value::Bool(true));
    assert_eq!(b["manual"], Value::Bool(false));
    assert_eq!(b["company_dependent"], Value::Bool(false));
    assert_eq!(b["readonly"], Value::Bool(false));

    let num = &fields["number"];
    assert_eq!(num["type"], Value::str("float"));
    assert_eq!(num["digits"], Value::list([Value::Int(0), Value::Int(2)]));

    let mon = &fields["monetary"];
    assert_eq!(mon["type"], Value::str("monetary"));
    assert_eq!(mon["currency_field"], Value::str("currency_id"));

    let c = &fields["char"];
    assert_eq!(c["type"], Value::str("char"));
    assert_eq!(c["trim"], Value::Bool(true));
    assert_eq!(c["translate"], Value::Bool(false));

    let sel = &fields["selection"];
    assert_eq!(sel["type"], Value::str("selection"));
    assert_eq!(
        sel["selection"],
        Value::list([Value::list([
            Value::str("option_1"),
            Value::str("Option 1")
        ])])
    );

    // The fixture declares a real callable provider; metadata must resolve it, not return an
    // empty placeholder. Callable value-conversion remains covered separately by FIELD-032.
    assert_eq!(fields["selection_str"]["selection"], sel["selection"]);

    drop(tx);
    h.cleanup().await.unwrap();
}

mod upgrade {
    use std::sync::Arc;

    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder, StaticValue};
    use erp_sdk::{Orm, SUPERUSER_ID, StaticPolicy};
    use erp_testkit::Harness;

    use crate::support::{one, rows};

    pub(crate) static UPGRADE: AddonDescriptor = AddonDescriptor {
        name: "test_upgrade",
        depends: &["base"],
        exact_only: false,
    };

    fn leak(decl: ModelDecl) -> &'static ModelDecl {
        Box::leak(Box::new(decl))
    }

    #[tokio::test]
    async fn spec_field_041_required_and_not_null() {
        let v1 = ModelDecl::new("test_upgrade", "x.upgrade").fields(
            const {
                &[
                    FieldDecl::char("code").required(true),
                    FieldDecl::char("note"),
                ]
            },
        );
        let h = Harness::builder([&UPGRADE])
            .with_decls([v1])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let nullable = "SELECT is_nullable::text FROM information_schema.columns \
                        WHERE table_name = 'x_upgrade' AND column_name = ";
        assert_eq!(
            one(&mut tx, &format!("{nullable}'code'")).await.as_deref(),
            Some("NO")
        );
        tx.batch("INSERT INTO x_upgrade (code) VALUES ('a'), ('b')")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // v2: code optional, note required with a default, new columns with defaults
        let v2 = leak(ModelDecl::new("test_upgrade", "x.upgrade").fields(
            const {
                &[
                    FieldDecl::char("code"),
                    FieldDecl::char("note")
                        .required(true)
                        .default(StaticValue::Str("n/a")),
                    FieldDecl::integer("qty").default(StaticValue::Int(5)),
                    FieldDecl::boolean("flag").default(StaticValue::Bool(false)),
                    FieldDecl::boolean("flag2").default(StaticValue::Bool(true)),
                    FieldDecl::char("plain"),
                ]
            },
        ));
        let registry = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v2])
                .build()
                .unwrap(),
        );
        let orm = Orm::builder(registry, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();
        assert_eq!(
            one(&mut tx, &format!("{nullable}'code'")).await.as_deref(),
            Some("YES")
        );
        assert_eq!(
            one(&mut tx, &format!("{nullable}'note'")).await.as_deref(),
            Some("NO")
        );
        let data = rows(
            &mut tx,
            "SELECT note, qty::text, flag::text, flag2::text, plain FROM x_upgrade ORDER BY id",
        )
        .await;
        let row = vec![
            Some("n/a".to_owned()),
            Some("5".to_owned()),
            None,
            Some("true".to_owned()),
            None,
        ];
        assert_eq!(data, vec![row.clone(), row]);
        tx.commit().await.unwrap();
        drop(tx);
        drop(orm);
        h.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn spec_field_042_initializing_new_column_on_existing_rows() {
        let v1 = ModelDecl::new("test_upgrade", "x.upgrade_rows")
            .fields(const { &[FieldDecl::char("name")] });
        let h = Harness::builder([&UPGRADE])
            .with_decls([v1])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.batch("INSERT INTO x_upgrade_rows (name) VALUES ('r1'), ('r2'), ('r3')")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // v2 adds qty=Integer(default=5), flag=Boolean(default=False), flag2=Boolean(default=True), note=Char()
        let v2 = leak(ModelDecl::new("test_upgrade", "x.upgrade_rows").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::integer("qty").default(StaticValue::Int(5)),
                    FieldDecl::boolean("flag").default(StaticValue::Bool(false)),
                    FieldDecl::boolean("flag2").default(StaticValue::Bool(true)),
                    FieldDecl::char("note"),
                ]
            },
        ));
        let registry = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v2])
                .build()
                .unwrap(),
        );
        let orm = Orm::builder(registry, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();

        let data = rows(
            &mut tx,
            "SELECT qty::text, flag::text, flag2::text, note FROM x_upgrade_rows ORDER BY id",
        )
        .await;
        let expected_row = vec![Some("5".to_owned()), None, Some("true".to_owned()), None];
        assert_eq!(
            data,
            vec![expected_row.clone(), expected_row.clone(), expected_row]
        );

        tx.commit().await.unwrap();
        drop(tx);
        drop(orm);
        h.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn spec_field_039_char_size_changes() {
        let v1 = ModelDecl::new("test_upgrade", "x.upgrade_char_size")
            .fields(const { &[FieldDecl::char("code").size(3)] });
        let h = Harness::builder([&UPGRADE])
            .with_decls([v1])
            .build()
            .await
            .unwrap();

        let mut tx = h.begin().await.unwrap();
        let col_len = "SELECT character_maximum_length::text FROM information_schema.columns \
                       WHERE table_name = 'x_upgrade_char_size' AND column_name = 'code'";
        assert_eq!(one(&mut tx, col_len).await.as_deref(), Some("3"));
        drop(tx);

        // (b) Field changes to size=2: keep-on-shrink, column stays varchar(3)
        let v2 = leak(
            ModelDecl::new("test_upgrade", "x.upgrade_char_size")
                .fields(const { &[FieldDecl::char("code").size(2)] }),
        );
        let registry2 = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v2])
                .build()
                .unwrap(),
        );
        let orm2 = Orm::builder(registry2, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let report = orm2.sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");

        let mut tx = h.begin().await.unwrap();
        assert_eq!(one(&mut tx, col_len).await.as_deref(), Some("3"));
        drop(tx);

        // (a) Field changes to size=10: column widens to varchar(10)
        let v3 = leak(
            ModelDecl::new("test_upgrade", "x.upgrade_char_size")
                .fields(const { &[FieldDecl::char("code").size(10)] }),
        );
        let registry3 = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v3])
                .build()
                .unwrap(),
        );
        let orm3 = Orm::builder(registry3, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let report = orm3.sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");

        let mut tx = h.begin().await.unwrap();
        assert_eq!(one(&mut tx, col_len).await.as_deref(), Some("10"));
        drop(tx);

        // (c) Field changes to unbounded (no size): column becomes unbounded varchar
        let v4 = leak(
            ModelDecl::new("test_upgrade", "x.upgrade_char_size")
                .fields(const { &[FieldDecl::char("code")] }),
        );
        let registry4 = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v4])
                .build()
                .unwrap(),
        );
        let orm4 = Orm::builder(registry4, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let report = orm4.sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");

        let mut tx = h.begin().await.unwrap();
        assert_eq!(one(&mut tx, col_len).await, None);
        drop(tx);

        h.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn spec_field_043_int4_to_varchar() {
        let v1 = ModelDecl::new("test_upgrade", "x.upgrade_col_type")
            .fields(const { &[FieldDecl::integer("code")] });
        let h = Harness::builder([&UPGRADE])
            .with_decls([v1])
            .build()
            .await
            .unwrap();

        let mut tx = h.begin().await.unwrap();
        tx.batch("INSERT INTO x_upgrade_col_type (code) VALUES (42), (100)")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Redefine code as Char()
        let v2 = leak(
            ModelDecl::new("test_upgrade", "x.upgrade_col_type")
                .fields(const { &[FieldDecl::char("code")] }),
        );
        let registry2 = Arc::new(
            RegistryBuilder::new([&UPGRADE])
                .with_decls([v2])
                .build()
                .unwrap(),
        );
        let orm2 = Orm::builder(registry2, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let report = orm2.sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");

        let mut tx = h.begin().await.unwrap();
        let udt = "SELECT udt_name::text FROM information_schema.columns \
                   WHERE table_name = 'x_upgrade_col_type' AND column_name = 'code'";
        assert_eq!(one(&mut tx, udt).await.as_deref(), Some("varchar"));

        let vals = rows(&mut tx, "SELECT code FROM x_upgrade_col_type ORDER BY id").await;
        assert_eq!(
            vals,
            vec![vec![Some("42".to_owned())], vec![Some("100".to_owned())]]
        );
        drop(tx);

        h.cleanup().await.unwrap();
    }
}
