//! Native optimizer identities; conversion is the SQL/ORM boundary's responsibility.
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_domain::{Domain, FieldInfo, FieldType, ModelContext, Operator, Schema, Value};
struct NativeSchema;
impl Schema for NativeSchema {
    fn field(&self, model: &str, name: &str) -> Option<FieldInfo<'_>> {
        if model != "native" {
            return None;
        }
        let (name, kind) = match name {
            "u" => ("u", FieldType::Uuid),
            "x" => ("x", FieldType::Xid8),
            "t" => ("t", FieldType::TimestampTz),
            _ => return None,
        };
        Some(FieldInfo::new(name, kind))
    }
    fn has_model(&self, model: &str) -> bool {
        model == "native"
    }
}

#[test]
fn native_inequality_complements_keep_null_without_rewriting_native_literals() {
    let context = ModelContext::new(&NativeSchema, "native");
    for (name, kind, text) in [
        ("u", FieldType::Uuid, "00000000-0000-0000-0000-000000000000"),
        ("x", FieldType::Xid8, "18446744073709551615"),
        (
            "t",
            FieldType::TimestampTz,
            "2024-03-01T01:00:00.123456+07:00",
        ),
    ] {
        assert!(!kind.has_falsy_value());
        assert!(!kind.is_relational());
        let greater = Domain::condition(name, Operator::Gt, Value::Str(text.into())).unwrap();
        let complement = (!greater).optimize(&context).unwrap();
        let expected = (Domain::condition(name, Operator::Le, Value::Str(text.into())).unwrap()
            | Domain::condition(name, Operator::In, Value::List(vec![Value::Bool(false)]))
                .unwrap())
        .optimize(&context)
        .unwrap();
        assert_eq!(complement, expected);
    }
}
