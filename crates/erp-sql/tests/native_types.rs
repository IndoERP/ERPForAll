//! Native SQL scalar lowering; no PostgreSQL execution or default-function claim.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_domain::{Domain, Operator, Value};
use erp_fields::{ErrorKind, FieldType, TimestampTz, Uuid, Xid8};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sql::filter::{Unaccent, WhereContext, compile_where};
use erp_sql::param::{Param, Scalar, ScalarKind, falsy_value, to_scalar};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "native_sql",
    depends: &["base"],
    exact_only: false,
};
static MODEL: ModelDecl = ModelDecl::new("native_sql", "native.sql").fields(
    const {
        &[
            FieldDecl::uuid("u"),
            FieldDecl::xid8("x"),
            FieldDecl::timestamptz("t"),
        ]
    },
);

#[test]
fn native_domain_literals_keep_native_types_and_no_concrete_null_default() {
    for (field, kind, text, expected) in [
        (
            FieldType::Uuid,
            ScalarKind::Uuid,
            "00000000-0000-0000-0000-000000000000",
            Scalar::Uuid(Uuid::NIL),
        ),
        (
            FieldType::Xid8,
            ScalarKind::Xid8,
            "18446744073709551615",
            Scalar::Xid8(Xid8::from_u64(u64::MAX)),
        ),
        (
            FieldType::TimestampTz,
            ScalarKind::TimestampTz,
            "2024-03-01T01:00:00.123456+07:00",
            Scalar::TimestampTz(TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap()),
        ),
    ] {
        assert_eq!(ScalarKind::of_field(&field), Some(kind));
        assert_eq!(to_scalar(kind, &Value::Str(text.into())).unwrap(), expected);
        assert_eq!(Param::Scalar(expected).sql_type(), field.type_name());
        assert_eq!(falsy_value(&field), None);
    }
    for wrong in [
        Value::Int(0),
        Value::Int(1),
        Value::Float(1.0),
        Value::Bool(false),
        Value::Str("-1".into()),
    ] {
        assert_eq!(
            to_scalar(ScalarKind::Xid8, &wrong),
            Err(erp_sql::param::NotConvertible)
        );
    }
    assert_eq!(
        to_scalar(
            ScalarKind::Uuid,
            &Value::Str("01234567-89AB-CDEF-0123-456789ABCDEF".into())
        ),
        Err(erp_sql::param::NotConvertible)
    );
}

#[test]
fn native_filters_bind_values_and_refuse_text_pattern_casts() {
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let context = WhereContext {
        registry: &registry,
        model: registry.model("native.sql").unwrap(),
        unaccent: Unaccent::Off,
        lang: None,
    };
    for (field, text, cast, expected) in [
        (
            "u",
            "00000000-0000-0000-0000-000000000000",
            "uuid",
            Scalar::Uuid(Uuid::NIL),
        ),
        (
            "x",
            "18446744073709551615",
            "xid8",
            Scalar::Xid8(Xid8::from_u64(u64::MAX)),
        ),
        (
            "t",
            "2024-02-29T18:00:00.123456Z",
            "timestamptz",
            Scalar::TimestampTz(TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap()),
        ),
    ] {
        let domain = Domain::condition(
            field,
            Operator::In,
            Value::List(vec![Value::Str(text.into())]),
        )
        .unwrap();
        let filter = compile_where(&domain, &context).unwrap();
        assert!(
            filter.sql.contains(&format!("$1::{cast}")),
            "{}",
            filter.sql
        );
        assert!(!filter.sql.contains(text));
        assert_eq!(
            filter.params,
            vec![Param::Array(expected.kind(), vec![expected])]
        );
        let domain =
            Domain::condition(field, Operator::ILike, Value::Str("anything".into())).unwrap();
        let error = compile_where(&domain, &context).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "operator 'ilike' is not supported for native field native.sql.{field} ({cast})"
            )
        );
        let external = Domain::from_value(&Value::List(vec![Value::List(vec![
            Value::Str(field.into()),
            Value::Str("ilike".into()),
            Value::Str("anything".into()),
        ])]))
        .unwrap();
        let external_error = compile_where(&external, &context).unwrap_err();
        assert_eq!(external_error.kind(), ErrorKind::Validation);
        assert_eq!(external_error.message(), error.message());
    }
}

#[test]
fn native_constant_predicates_emit_only_validated_typed_literals() {
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let model = registry.model("native.sql").unwrap();
    for (field, input, expected, cast) in [
        (
            "u",
            "00000000-0000-0000-0000-000000000000",
            Uuid::NIL.to_string(),
            "uuid",
        ),
        (
            "x",
            "18446744073709551615",
            Xid8::from_u64(u64::MAX).to_string(),
            "xid8",
        ),
        (
            "t",
            "2024-03-01T01:00:00.123456+07:00",
            TimestampTz::parse("2024-02-29T18:00:00.123456Z")
                .unwrap()
                .to_string(),
            "timestamptz",
        ),
    ] {
        let sql =
            erp_sql::constant_filter::compile(&format!("[('{field}', '=', '{input}')]"), model)
                .unwrap();
        assert_eq!(sql, format!("\"{field}\" = '{expected}'::{cast}"));
        let sql = erp_sql::constant_filter::compile(&format!("[('{field}', '=', False)]"), model)
            .unwrap();
        assert_eq!(sql, format!("\"{field}\" IS NULL"));
    }
    assert!(erp_sql::constant_filter::compile("[('x', '=', 1)]", model).is_err());
    assert!(erp_sql::constant_filter::compile("[('u', '=', \"' OR TRUE --\")]", model).is_err());
    assert!(
        erp_sql::constant_filter::compile("[('t', '=', '2024-01-01 00:00:00')]", model).is_err()
    );
}
