//! Native field declarations and runtime-owned mirrors; no database/default generation claim.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{ColumnType, FieldType};
use erp_registry::{
    AddonDescriptor, FieldDecl, FieldDef, KindDef, ModelDecl, ModelDef, Overlay, RegistryBuilder,
};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "native_types",
    depends: &["base"],
    exact_only: false,
};
static MODEL: ModelDecl = ModelDecl::new("native_types", "native.item").fields(
    const {
        &[
            FieldDecl::uuid("u"),
            FieldDecl::xid8("x"),
            FieldDecl::timestamptz("t"),
            FieldDecl::datetime("legacy"),
        ]
    },
);

#[test]
fn native_static_declarations_keep_distinct_storage_types() {
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let model = registry.model("native.item").unwrap();
    for (name, kind, column) in [
        ("u", FieldType::Uuid, ColumnType::Uuid),
        ("x", FieldType::Xid8, ColumnType::Xid8),
        ("t", FieldType::TimestampTz, ColumnType::TimestampTz),
        ("legacy", FieldType::Datetime, ColumnType::Timestamp),
    ] {
        let field = model.field(name).unwrap();
        assert_eq!(field.ty, kind);
        assert_eq!(field.column(), Some(column));
        assert!(!field.server_set);
    }
}

#[test]
fn owned_native_declarations_do_not_become_char_or_signed_fields() {
    let mut overlay = Overlay::default();
    overlay
        .add_addon_defs([ModelDef::new("native_types", "native.owned")
            .field(FieldDef::new("u", KindDef::Uuid))
            .field(FieldDef::new("x", KindDef::Xid8))
            .field(FieldDef::new("t", KindDef::TimestampTz))])
        .unwrap();
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap();
    let model = registry.model("native.owned").unwrap();
    assert_eq!(model.field("u").unwrap().ty, FieldType::Uuid);
    assert_eq!(model.field("x").unwrap().ty, FieldType::Xid8);
    assert_eq!(model.field("t").unwrap().ty, FieldType::TimestampTz);
}

#[test]
fn native_literal_defaults_require_an_explicit_future_contract() {
    use erp_fields::ErrorKind;
    use erp_registry::StaticValue;
    for (field, literal) in [
        (FieldDecl::uuid("u"), "00000000-0000-0000-0000-000000000001"),
        (FieldDecl::xid8("x"), "18446744073709551615"),
        (FieldDecl::timestamptz("t"), "2026-11-01T12:34:56.123456Z"),
    ] {
        let model = Box::leak(Box::new(
            ModelDecl::new("native_types", "native.defaults").fields(Box::leak(
                vec![field.default(StaticValue::Str(literal))].into_boxed_slice(),
            )),
        ));
        let error = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([&*model])
            .build()
            .expect_err("literal native defaults must refuse until their schema contract exists");
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(
            error
                .message()
                .contains("future native static-default slice"),
            "{error}"
        );
        assert!(error.message().contains(field.name), "{error}");
        for null in [
            field.no_default(),
            field.default(StaticValue::False),
            field.default(StaticValue::Bool(false)),
        ] {
            let model = Box::leak(Box::new(
                ModelDecl::new("native_types", "native.defaults")
                    .fields(Box::leak(vec![null].into_boxed_slice())),
            ));
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls([&*model])
                .build()
                .unwrap();
        }
    }
}
