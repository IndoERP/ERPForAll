// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `db_default` field attribute at registry build: which declarations may keep their
//! static default as the column's database default. Pure: no database.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder, StaticValue};

static FLAG: AddonDescriptor = AddonDescriptor {
    name: "flag",
    depends: &["base"],
    exact_only: false,
};
static FLAG_EXT: AddonDescriptor = AddonDescriptor {
    name: "flag_ext",
    depends: &["flag"],
    exact_only: false,
};

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&FLAG, &FLAG_EXT])
        .with_decls(decls)
        .build()
}

fn model(fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("flag", "flag.x").fields(Vec::leak(fields))
}

fn refusal(field: FieldDecl) -> (ErrorKind, String) {
    let err = build(vec![model(vec![field])]).unwrap_err();
    (err.kind(), err.message().to_owned())
}

const NEEDS: &str = "db_default is for a stored, required boolean field with a static \
                     boolean default, not company-dependent and not generated";

fn expected(why: &str) -> String {
    format!("field 'flag.x.escape': {NEEDS}; this field {why}")
}

/// A stored required boolean with a static default keeps the flag; a field without it does not.
#[test]
fn a_required_boolean_with_a_static_default_takes_the_flag() {
    let registry = build(vec![model(vec![
        FieldDecl::boolean("escape")
            .required(true)
            .default(StaticValue::Bool(true))
            .db_default(true),
        FieldDecl::boolean("closed")
            .required(true)
            .default(StaticValue::Bool(false))
            .db_default(true),
        FieldDecl::boolean("plain")
            .required(true)
            .default(StaticValue::Bool(true)),
    ])])
    .unwrap();
    let spec = registry.model("flag.x").unwrap();
    assert!(spec.field("escape").unwrap().db_default);
    assert!(spec.field("closed").unwrap().db_default);
    assert!(!spec.field("plain").unwrap().db_default);
}

/// The shape of `res.users.share`: a stored compute with a static default keeps the flag.
#[test]
fn a_stored_compute_with_a_static_default_takes_the_flag() {
    let registry = build(vec![model(vec![
        FieldDecl::boolean("escape")
            .compute("_compute_escape")
            .store(true)
            .required(true)
            .default(StaticValue::Bool(true))
            .db_default(true),
    ])])
    .unwrap();
    assert!(
        registry
            .model("flag.x")
            .unwrap()
            .field("escape")
            .unwrap()
            .db_default
    );
}

/// Every declaration outside the admitted shape is refused, naming the field and what is missing.
#[test]
fn every_other_declaration_is_refused() {
    let cases = [
        (
            FieldDecl::integer("escape")
                .required(true)
                .default(StaticValue::Int(1))
                .db_default(true),
            "is integer",
        ),
        (
            FieldDecl::boolean("escape")
                .default(StaticValue::Bool(true))
                .db_default(true),
            "is not required",
        ),
        (
            FieldDecl::boolean("escape").required(true).db_default(true),
            "has no static boolean default",
        ),
        (
            FieldDecl::boolean("escape")
                .required(true)
                .default(StaticValue::False)
                .db_default(true),
            "has no static boolean default",
        ),
        (
            FieldDecl::boolean("escape")
                .compute("_compute_escape")
                .required(true)
                .default(StaticValue::Bool(true))
                .db_default(true),
            "is not stored",
        ),
        (
            FieldDecl::boolean("escape")
                .company_dependent(true)
                .required(true)
                .default(StaticValue::Bool(true))
                .db_default(true),
            "is company-dependent",
        ),
    ];
    for (field, why) in cases {
        assert_eq!(
            refusal(field),
            (ErrorKind::Programming, expected(why)),
            "{why}"
        );
    }
}

/// An extension may switch the flag off (schema sync then drops the column default) and on again.
#[test]
fn an_extension_switches_the_flag_off_and_on() {
    let base = model(vec![
        FieldDecl::boolean("escape")
            .required(true)
            .default(StaticValue::Bool(true))
            .db_default(true),
    ]);
    let off = ModelDecl::new("flag_ext", "flag.x")
        .inherit(&["flag.x"])
        .fields(const { &[FieldDecl::boolean("escape").db_default(false)] });
    let registry = build(vec![base, off]).unwrap();
    assert!(
        !registry
            .model("flag.x")
            .unwrap()
            .field("escape")
            .unwrap()
            .db_default
    );

    let plain = model(vec![
        FieldDecl::boolean("escape")
            .required(true)
            .default(StaticValue::Bool(true)),
    ]);
    let on = ModelDecl::new("flag_ext", "flag.x")
        .inherit(&["flag.x"])
        .fields(const { &[FieldDecl::boolean("escape").db_default(true)] });
    let registry = build(vec![plain, on]).unwrap();
    assert!(
        registry
            .model("flag.x")
            .unwrap()
            .field("escape")
            .unwrap()
            .db_default
    );
}

/// The `_inherits` copy of a flagged field has no column, so it does not carry the flag; the
/// parent's field keeps it.
#[test]
fn the_inherits_copy_does_not_carry_the_flag() {
    let parent = ModelDecl::new("flag", "flag.parent").fields(
        const {
            &[FieldDecl::boolean("escape")
                .required(true)
                .default(StaticValue::Bool(true))
                .db_default(true)]
        },
    );
    let child = ModelDecl::new("flag", "flag.child")
        .inherits(&[("flag.parent", "parent_id")])
        .fields(
            const {
                &[FieldDecl::many2one("parent_id", "flag.parent")
                    .required(true)
                    .ondelete(OnDelete::Cascade)]
            },
        );
    let registry = build(vec![parent, child]).unwrap();
    assert!(
        registry
            .model("flag.parent")
            .unwrap()
            .field("escape")
            .unwrap()
            .db_default
    );
    let copy = registry
        .model("flag.child")
        .unwrap()
        .field("escape")
        .unwrap();
    assert!(copy.inherited.is_some());
    assert!(!copy.db_default);
}
