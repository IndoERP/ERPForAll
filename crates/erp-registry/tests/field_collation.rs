// SPDX-License-Identifier: MIT OR Apache-2.0
//! Closed field collation declarations and physical ownership.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{ErrorKind, TextCollation};
use erp_registry::{
    AddonDescriptor, FieldDecl, FieldDef, KindDef, ModelDecl, ModelDef, Overlay, Registry,
    RegistryBuilder,
};
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "collation",
    depends: &["base"],
    exact_only: false,
};
static EXT: AddonDescriptor = AddonDescriptor {
    name: "collation_ext",
    depends: &["collation"],
    exact_only: false,
};
fn build(fields: Vec<FieldDecl>, extension: Vec<FieldDecl>) -> erp_fields::Result<Registry> {
    let model = Box::leak(Box::new(
        ModelDecl::new("collation", "collation.item").fields(Vec::leak(fields)),
    ));
    let ext = Box::leak(Box::new(
        ModelDecl::extension("collation_ext", "collation.item", &["collation.item"])
            .fields(Vec::leak(extension)),
    ));
    RegistryBuilder::new([&ADDON, &EXT])
        .linked(false)
        .with_decls([&*model, &*ext])
        .build()
}
#[test]
fn declaration_inherits_and_explicit_default_resets() {
    for (extension, expected) in [
        (vec![], TextCollation::C),
        (
            vec![FieldDecl::char("code").string("Renamed")],
            TextCollation::C,
        ),
        (
            vec![FieldDecl::char("code").collation("default")],
            TextCollation::DatabaseDefault,
        ),
    ] {
        let r = build(
            vec![
                FieldDecl::char("code").collation("C"),
                FieldDecl::text("plain"),
            ],
            extension,
        )
        .unwrap();
        let m = r.model("collation.item").unwrap();
        assert_eq!(m.field("code").unwrap().collation, expected);
        assert_eq!(
            m.field("plain").unwrap().collation,
            TextCollation::DatabaseDefault
        );
    }
}
#[test]
fn invalid_names_and_non_text_storage_are_refused() {
    for value in [
        "",
        "c",
        "POSIX",
        "en_US",
        "und-x-icu",
        "C\0",
        "C\"; DROP TABLE x",
    ] {
        let e = build(vec![FieldDecl::char("code").collation(value)], vec![]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            format!(
                "field 'collation.item.code': unsupported collation {value:?}; expected default or C; locale/ICU collations require a later release"
            )
        );
    }
    for field in [
        FieldDecl::integer("code"),
        FieldDecl::html("code"),
        FieldDecl::char("code").translate(true),
        FieldDecl::char("code").company_dependent(true),
        FieldDecl::char("code").compute("_code"),
    ] {
        let e = build(vec![field.collation("C")], vec![]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "field 'collation.item.code': collation requires a stored Char/Text SQL column, not translated or company-dependent"
        );
    }
    let e = build(
        vec![FieldDecl::char("code").collation("invalid")],
        vec![FieldDecl::char("code").collation("C")],
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "field 'collation.item.code': unsupported collation \"invalid\"; expected default or C; locale/ICU collations require a later release"
    );
}
#[test]
fn owned_declarations_keep_collation() {
    let mut overlay = Overlay::default();
    overlay
        .add_addon_defs([ModelDef::new("collation", "collation.owned")
            .field(FieldDef::new("code", KindDef::Text).collation("C"))])
        .unwrap();
    let r = RegistryBuilder::new([&ADDON])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap();
    assert_eq!(
        r.model("collation.owned")
            .unwrap()
            .field("code")
            .unwrap()
            .collation,
        TextCollation::C
    );
}
#[test]
fn foreign_addon_cannot_change_column_collation() {
    let model = Box::leak(Box::new(
        ModelDecl::new("collation", "collation.locked")
            .owner_only_stored_fields(&[])
            .fields(Vec::leak(vec![FieldDecl::char("code")])),
    ));
    let ext = Box::leak(Box::new(
        ModelDecl::extension("collation_ext", "collation.locked", &["collation.locked"])
            .fields(Vec::leak(vec![FieldDecl::char("code").collation("C")])),
    ));
    let e = RegistryBuilder::new([&ADDON, &EXT])
        .linked(false)
        .with_decls([&*model, &*ext])
        .build()
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "addon 'collation_ext' cannot add or change stored field 'collation.locked.code'; owner is 'collation'; declared by model definition 'collation.locked' of addon 'collation_ext'"
    );
}

#[test]
fn owner_and_coowner_can_change_column_collation() {
    for author in ["collation", "collation_ext"] {
        let model = Box::leak(Box::new(
            ModelDecl::new("collation", "collation.permitted")
                .owner_only_stored_fields(&["collation_ext"])
                .fields(Vec::leak(vec![FieldDecl::char("code")])),
        ));
        let ext = Box::leak(Box::new(
            ModelDecl::extension(author, "collation.permitted", &["collation.permitted"])
                .fields(Vec::leak(vec![FieldDecl::char("code").collation("C")])),
        ));
        let registry = RegistryBuilder::new([&ADDON, &EXT])
            .linked(false)
            .with_decls([&*model, &*ext])
            .build()
            .unwrap();
        assert_eq!(
            registry
                .model("collation.permitted")
                .unwrap()
                .field("code")
                .unwrap()
                .collation,
            TextCollation::C
        );
    }
}
