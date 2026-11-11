//! Exact-only addon admission is checked after complete model composition.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{Digits, ErrorKind};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

const BINARY: &[FieldDecl] = &[FieldDecl::float("qty")];
const NUMERIC: &[FieldDecl] = &[
    FieldDecl::float_digits("qty", Digits::Named("Product Unit")),
    FieldDecl::float_digits("cost", Digits::Unrounded),
    FieldDecl::float_digits(
        "fixed",
        Digits::Fixed {
            precision: 16,
            scale: 2,
        },
    ),
];

fn addon(name: &'static str) -> &'static AddonDescriptor {
    Box::leak(Box::new(AddonDescriptor {
        name,
        depends: &["base"],
        exact_only: false,
    }))
}

#[test]
fn mandatory_addons_refuse_binary_columns() {
    for name in [
        "account",
        "account_extra",
        "stock",
        "stock_extra",
        "product",
        "uom",
    ] {
        let decl = Box::leak(Box::new(ModelDecl::new(name, "exact.item").fields(BINARY)));
        let error = RegistryBuilder::new([addon(name)])
            .linked(false)
            .with_decls([&*decl])
            .build()
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "Exact-only addon '{name}' refuses binary-float column 'exact.item.qty' (protecting model definition 'exact.item' of addon '{name}'; declaring model definition 'exact.item' of addon '{name}'; physical column 'exact.item.qty'); declare numeric digits instead."
            )
        );
    }
}

#[test]
fn unrelated_addons_keep_binary_columns_and_exact_alternatives_are_admitted() {
    for name in ["ordinary", "product_extra", "uom_extra"] {
        let decl = Box::leak(Box::new(ModelDecl::new(name, "exact.item").fields(BINARY)));
        RegistryBuilder::new([addon(name)])
            .linked(false)
            .with_decls([&*decl])
            .build()
            .unwrap();
    }
    let decl = ModelDecl::new("stock", "exact.item").fields(NUMERIC);
    RegistryBuilder::new([addon("stock")])
        .linked(false)
        .with_decls([&*Box::leak(Box::new(decl))])
        .build()
        .unwrap();
}

#[test]
fn ordinary_extension_cannot_add_float_to_protected_model() {
    let root = ModelDecl::new("stock", "exact.item");
    let ext = ModelDecl::extension("ordinary", "exact.item", &["exact.item"])
        .fields(BINARY)
        .at("ordinary.rs", 42);
    let error = RegistryBuilder::new([
        addon("stock"),
        &*Box::leak(Box::new(AddonDescriptor {
            name: "ordinary",
            depends: &["stock"],
            exact_only: false,
        })),
    ])
    .linked(false)
    .with_decls([&*Box::leak(Box::new(root)), &*Box::leak(Box::new(ext))])
    .build()
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'exact.item.qty' (protecting model definition 'exact.item' of addon 'stock'; declaring model definition 'exact.item' of addon 'ordinary' (ordinary.rs:42); physical column 'exact.item.qty'); declare numeric digits instead."
    );
}

#[test]
fn explicit_opt_in_protects_an_ordinary_addon() {
    let owner = Box::leak(Box::new(AddonDescriptor {
        name: "ordinary",
        depends: &["base"],
        exact_only: true,
    }));
    let decl = Box::leak(Box::new(
        ModelDecl::new("ordinary", "exact.item").fields(BINARY),
    ));
    let error = RegistryBuilder::new([&*owner])
        .linked(false)
        .with_decls([&*decl])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'ordinary' refuses binary-float column 'exact.item.qty' (protecting model definition 'exact.item' of addon 'ordinary'; declaring model definition 'exact.item' of addon 'ordinary'; physical column 'exact.item.qty'); declare numeric digits instead."
    );
}

#[test]
fn interned_descriptors_include_numeric_policy() {
    let ordinary =
        erp_registry::intern_addon_with_policy("numeric_policy", &["base"], false).unwrap();
    let exact = erp_registry::intern_addon_with_policy("numeric_policy", &["base"], true).unwrap();
    assert!(!std::ptr::eq(ordinary, exact));
    assert!(!ordinary.is_exact_only());
    assert!(exact.is_exact_only());
    assert!(std::ptr::eq(
        exact,
        erp_registry::intern_addon_with_policy("numeric_policy", &["base"], true).unwrap()
    ));
}

#[test]
fn stored_related_float_is_checked_after_resolution() {
    const SOURCE: ModelDecl =
        ModelDecl::new("ordinary", "ordinary.source").fields(&[FieldDecl::float("qty")]);
    const COPY: ModelDecl = ModelDecl::new("stock", "stock.copy").fields(&[
        FieldDecl::many2one("source_id", "ordinary.source"),
        FieldDecl::float("copy")
            .related("source_id.qty")
            .store(true),
    ]);
    let error = RegistryBuilder::new([addon("ordinary"), addon("stock")])
        .linked(false)
        .with_decls([&SOURCE, &COPY])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'stock.copy.copy' (protecting model definition 'stock.copy' of addon 'stock'; declaring model definition 'stock.copy' of addon 'stock'; physical column 'stock.copy.copy'); declare numeric digits instead."
    );
}

#[test]
fn ordinary_classical_child_keeps_exact_inherited_fields_without_protecting_new_ordinary_fields() {
    const SOURCE: ModelDecl = ModelDecl::new("stock", "stock.parent").fields(NUMERIC);
    const CHILD: ModelDecl = ModelDecl::new("ordinary", "ordinary.child")
        .inherit(&["stock.parent"])
        .fields(&[FieldDecl::float("extra")]);
    let ordinary = Box::leak(Box::new(AddonDescriptor {
        name: "ordinary",
        depends: &["stock"],
        exact_only: false,
    }));
    let registry = RegistryBuilder::new([addon("stock"), &*ordinary])
        .linked(false)
        .with_decls([&SOURCE, &CHILD])
        .build()
        .unwrap();
    let child = registry.model("ordinary.child").unwrap();
    assert_eq!(
        child.field("extra").unwrap().column(),
        Some(erp_fields::ColumnType::Float8)
    );
    assert_eq!(
        child.field("qty").unwrap().column(),
        Some(erp_fields::ColumnType::Numeric)
    );
}

#[test]
fn exact_extensions_preserve_unrelated_base_float_for_method_only_extension() {
    const ROOT: ModelDecl = ModelDecl::new("ordinary", "ordinary.item").fields(BINARY);
    const EXT: ModelDecl = ModelDecl::extension("stock", "ordinary.item", &["ordinary.item"]);
    let exact = Box::leak(Box::new(AddonDescriptor {
        name: "stock",
        depends: &["ordinary"],
        exact_only: true,
    }));
    let registry = RegistryBuilder::new([addon("ordinary"), &*exact])
        .linked(false)
        .with_decls([&ROOT, &EXT])
        .build()
        .unwrap();
    assert_eq!(
        registry
            .model("ordinary.item")
            .unwrap()
            .field("qty")
            .unwrap()
            .column(),
        Some(erp_fields::ColumnType::Float8)
    );
}

#[test]
fn exact_extensions_preserve_unrelated_base_float_when_adding_exact_fields() {
    const ROOT: ModelDecl = ModelDecl::new("ordinary", "ordinary.item").fields(BINARY);
    const EXT: ModelDecl = ModelDecl::extension("stock", "ordinary.item", &["ordinary.item"])
        .fields(&[FieldDecl::char("label")]);
    let exact = Box::leak(Box::new(AddonDescriptor {
        name: "stock",
        depends: &["ordinary"],
        exact_only: true,
    }));
    let registry = RegistryBuilder::new([addon("ordinary"), &*exact])
        .linked(false)
        .with_decls([&ROOT, &EXT])
        .build()
        .unwrap();
    assert_eq!(
        registry
            .model("ordinary.item")
            .unwrap()
            .field("qty")
            .unwrap()
            .column(),
        Some(erp_fields::ColumnType::Float8)
    );
    assert!(
        registry
            .model("ordinary.item")
            .unwrap()
            .field("label")
            .is_some()
    );
}

#[test]
fn exact_extension_cannot_declare_binary_float_on_an_ordinary_model() {
    const ROOT: ModelDecl = ModelDecl::new("ordinary", "ordinary.item").fields(BINARY);
    const EXT: ModelDecl = ModelDecl::extension("stock", "ordinary.item", &["ordinary.item"])
        .fields(&[FieldDecl::float("new_qty")]);
    let exact = Box::leak(Box::new(AddonDescriptor {
        name: "stock",
        depends: &["ordinary"],
        exact_only: true,
    }));
    let error = RegistryBuilder::new([addon("ordinary"), &*exact])
        .linked(false)
        .with_decls([&ROOT, &EXT])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'ordinary.item.new_qty' (protecting model definition 'ordinary.item' of addon 'stock'; declaring model definition 'ordinary.item' of addon 'stock'; physical column 'ordinary.item.new_qty'); declare numeric digits instead."
    );
}

#[test]
fn later_extension_cannot_materialize_an_exact_declared_field_as_binary_float() {
    const ROOT: ModelDecl = ModelDecl::new("ordinary", "ordinary.item");
    const EXACT: ModelDecl = ModelDecl::extension("stock", "ordinary.item", &["ordinary.item"])
        .fields(&[FieldDecl::float("qty").store(false)]);
    const LATER: ModelDecl = ModelDecl::extension("later", "ordinary.item", &["ordinary.item"])
        .fields(&[FieldDecl::float("qty").store(true)]);
    let exact = Box::leak(Box::new(AddonDescriptor {
        name: "stock",
        depends: &["ordinary"],
        exact_only: true,
    }));
    let later = Box::leak(Box::new(AddonDescriptor {
        name: "later",
        depends: &["stock"],
        exact_only: false,
    }));
    let registry = RegistryBuilder::new([addon("ordinary"), &*exact])
        .linked(false)
        .with_decls([&ROOT, &EXACT])
        .build()
        .unwrap();
    assert_eq!(
        registry
            .model("ordinary.item")
            .unwrap()
            .field("qty")
            .unwrap()
            .column(),
        None
    );
    let error = RegistryBuilder::new([addon("ordinary"), &*exact, &*later])
        .linked(false)
        .with_decls([&ROOT, &EXACT, &LATER])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'ordinary.item.qty' (protecting model definition 'ordinary.item' of addon 'stock'; declaring model definition 'ordinary.item' of addon 'later'; physical column 'ordinary.item.qty'); declare numeric digits instead."
    );
}

#[test]
fn delegated_float_cannot_bypass_protection_through_an_unprotected_parent() {
    const SOURCE: ModelDecl = ModelDecl::new("ordinary", "ordinary.parent").fields(BINARY);
    const CHILD: ModelDecl = ModelDecl::new("stock", "stock.delegate")
        .inherits(&[("ordinary.parent", "parent_id")])
        .fields(&[FieldDecl::many2one("parent_id", "ordinary.parent")
            .delegate(true)
            .required(true)
            .ondelete(erp_fields::OnDelete::Cascade)]);
    let error = RegistryBuilder::new([addon("ordinary"), addon("stock")])
        .linked(false)
        .with_decls([&SOURCE, &CHILD])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'stock.delegate.qty' (protecting model definition 'stock.delegate' of addon 'stock'; declaring model definition 'ordinary.parent' of addon 'ordinary'; physical column 'ordinary.parent.qty'); declare numeric digits instead."
    );
}

#[test]
fn manual_overlay_float_obeys_strict_and_drop_broken_policies() {
    use erp_registry::{DropReason, FieldDef, KindDef, ManualFieldPolicy, Overlay};
    const MODEL: ModelDecl = ModelDecl::new("stock", "exact.item");
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "exact.item",
            FieldDef::new("x_qty", KindDef::Float { digits: None }),
        )
        .unwrap();
    overlay
        .add_manual_field(
            "exact.item",
            FieldDef::new("x_label", KindDef::Char { size: None }),
        )
        .unwrap();
    let build = |policy| {
        RegistryBuilder::new([addon("stock")])
            .linked(false)
            .with_decls([&MODEL])
            .overlay(&overlay)
            .manual_fields(policy)
            .build()
    };
    let error = build(ManualFieldPolicy::Strict).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'exact.item.x_qty' (protecting model definition 'exact.item' of addon 'stock'; declaring model definition 'exact.item' of addon '(manual)'; physical column 'exact.item.x_qty'); declare numeric digits instead."
    );
    let registry = build(ManualFieldPolicy::DropBroken).unwrap();
    let model = registry.model("exact.item").unwrap();
    assert!(model.field("x_qty").is_none());
    assert!(model.field("x_label").is_some());
    assert_eq!(registry.dropped_fields().len(), 1);
    let dropped = &registry.dropped_fields()[0];
    assert_eq!((dropped.model(), dropped.field()), ("exact.item", "x_qty"));
    assert_eq!(dropped.reason(), &DropReason::Refused);
    assert_eq!(dropped.error().kind(), error.kind());
    assert_eq!(dropped.error().message(), error.message());
}

#[test]
fn delegated_manual_float_obeys_strict_and_drop_broken_policies() {
    use erp_registry::{DropReason, FieldDef, KindDef, ManualFieldPolicy, Overlay};
    const SOURCE: ModelDecl = ModelDecl::new("ordinary", "ordinary.parent");
    const CHILD: ModelDecl = ModelDecl::new("stock", "stock.delegate")
        .inherits(&[("ordinary.parent", "parent_id")])
        .fields(&[FieldDecl::many2one("parent_id", "ordinary.parent")
            .delegate(true)
            .required(true)
            .ondelete(erp_fields::OnDelete::Cascade)]);
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "ordinary.parent",
            FieldDef::new("x_qty", KindDef::Float { digits: None }),
        )
        .unwrap();
    overlay
        .add_manual_field(
            "ordinary.parent",
            FieldDef::new("x_label", KindDef::Char { size: None }),
        )
        .unwrap();
    let build = |policy| {
        RegistryBuilder::new([addon("ordinary"), addon("stock")])
            .linked(false)
            .with_decls([&SOURCE, &CHILD])
            .overlay(&overlay)
            .manual_fields(policy)
            .build()
    };
    let error = build(ManualFieldPolicy::Strict).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'stock.delegate.x_qty' (protecting model definition 'stock.delegate' of addon 'stock'; declaring model definition 'ordinary.parent' of addon '(manual)'; physical column 'ordinary.parent.x_qty'); declare numeric digits instead."
    );
    let registry = build(ManualFieldPolicy::DropBroken).unwrap();
    let parent = registry.model("ordinary.parent").unwrap();
    let child = registry.model("stock.delegate").unwrap();
    assert!(parent.field("x_qty").is_none());
    assert!(child.field("x_qty").is_none());
    assert!(parent.field("x_label").is_some());
    assert!(child.field("x_label").is_some());
    assert_eq!(registry.dropped_fields().len(), 1);
    let dropped = &registry.dropped_fields()[0];
    assert_eq!(
        (dropped.model(), dropped.field()),
        ("ordinary.parent", "x_qty")
    );
    assert_eq!(dropped.reason(), &DropReason::Refused);
    assert_eq!(dropped.error().kind(), error.kind());
    assert_eq!(dropped.error().message(), error.message());
}

#[test]
fn addon_float_remains_fatal_under_drop_broken() {
    const MODEL: ModelDecl = ModelDecl::new("stock", "exact.item").fields(BINARY);
    let error = RegistryBuilder::new([addon("stock")])
        .linked(false)
        .with_decls([&MODEL])
        .manual_fields(erp_registry::ManualFieldPolicy::DropBroken)
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Exact-only addon 'stock' refuses binary-float column 'exact.item.qty' (protecting model definition 'exact.item' of addon 'stock'; declaring model definition 'exact.item' of addon 'stock'; physical column 'exact.item.qty'); declare numeric digits instead."
    );
}

#[test]
fn bare_float_redeclaration_preserves_fixed_and_named_numeric_storage() {
    for digits in [
        Digits::Named("Product Unit"),
        Digits::Fixed {
            precision: 16,
            scale: 2,
        },
    ] {
        for exact_owned in [false, true] {
            let root = Box::leak(Box::new(ModelDecl::new(
                if exact_owned { "stock" } else { "ordinary" },
                "exact.item",
            )));
            let fields = Box::leak(Box::new([FieldDecl::float_digits("qty", digits)]));
            let exact = Box::leak(Box::new(
                ModelDecl::extension("stock", "exact.item", &["exact.item"]).fields(fields),
            ));
            let later = Box::leak(Box::new(
                ModelDecl::extension("later", "exact.item", &["exact.item"]).fields(BINARY),
            ));
            let exact_addon = Box::leak(Box::new(AddonDescriptor {
                name: "stock",
                depends: if exact_owned {
                    &["base"]
                } else {
                    &["ordinary"]
                },
                exact_only: true,
            }));
            let later_addon = Box::leak(Box::new(AddonDescriptor {
                name: "later",
                depends: &["stock"],
                exact_only: false,
            }));
            let registry = RegistryBuilder::new([addon("ordinary"), &*exact_addon, &*later_addon])
                .linked(false)
                .with_decls([&*root, &*exact, &*later])
                .build()
                .unwrap();
            let field = registry.model("exact.item").unwrap().field("qty").unwrap();
            assert_eq!(field.column(), Some(erp_fields::ColumnType::Numeric));
            assert_eq!(
                field.ty,
                erp_fields::FieldType::Float {
                    digits: Some(digits)
                }
            );
        }
    }
}
