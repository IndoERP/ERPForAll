// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner-only generated input declaration boundary (Proposed).
#![allow(clippy::unwrap_used, clippy::panic, reason = "acceptance tests")]
use erp_fields::ErrorKind;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

static BASE: AddonDescriptor = AddonDescriptor {
    name: "base",
    depends: &[],
    exact_only: false,
};
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "input_owner",
    depends: &["base"],
    exact_only: false,
};
static EXT: AddonDescriptor = AddonDescriptor {
    name: "input_ext",
    depends: &["input_owner"],
    exact_only: false,
};

fn build(fields: Vec<FieldDecl>, extension: Option<FieldDecl>) -> erp_fields::Result<Registry> {
    let fields = Box::leak(fields.into_boxed_slice());
    let owner = Box::leak(Box::new(
        ModelDecl::new("input_owner", "input.sample").fields(fields),
    ));
    let mut builder = RegistryBuilder::new([&BASE, &ADDON, &EXT])
        .linked(false)
        .with_decls([&*owner]);
    if let Some(field) = extension {
        let fields = Box::leak(vec![field].into_boxed_slice());
        let extension = Box::leak(Box::new(
            ModelDecl::new("input_ext", "input.sample")
                .inherit(&["input.sample"])
                .fields(fields),
        ));
        builder = builder.with_decls([&*extension]);
    }
    builder.build()
}
fn refuses(result: erp_fields::Result<Registry>, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), message);
}

#[test]
fn flag_on_ordinary_field_is_refused() {
    refuses(
        build(vec![FieldDecl::integer("source").input_adapted(true)], None),
        "Invalid generated input field 'input.sample.source': input_adapted requires a generated field",
    );
}

#[test]
fn flagged_field_requires_adapter_declaration() {
    refuses(
        build(
            vec![
                FieldDecl::integer("source"),
                FieldDecl::integer("total")
                    .generated("source + 1")
                    .input_adapted(true),
            ],
            None,
        ),
        "Model 'input.sample' has input-adapted generated fields but no _adapt_generated_input declaration",
    );
}

#[test]
fn extension_cannot_set_or_clear_input_flag() {
    for value in [false, true] {
        refuses(
            build(
                vec![
                    FieldDecl::integer("source"),
                    FieldDecl::integer("total").generated("source + 1"),
                ],
                Some(FieldDecl::integer("total").input_adapted(value)),
            ),
            "Invalid generated input field 'input.sample.total': only declaring addon 'input_owner' may set input_adapted; addon 'input_ext' cannot set or clear it",
        );
    }
}

#[test]
fn ordinary_generated_fields_keep_assignment_refusal() {
    let registry = build(
        vec![
            FieldDecl::integer("source"),
            FieldDecl::integer("total").generated("source + 1"),
        ],
        None,
    )
    .unwrap();
    let field = registry
        .model("input.sample")
        .unwrap()
        .field("total")
        .unwrap();
    assert!(!field.is_input_adapted());
    assert!(!field.is_assignment_eligible());
    assert!(field.readonly);
}

#[test]
fn declared_adapter_preserves_database_ownership() {
    use erp_registry::GeneratedInputAdapterDecl;
    static ADAPTER: GeneratedInputAdapterDecl =
        GeneratedInputAdapterDecl::new("input_owner", "input.sample");
    static FIELDS: &[FieldDecl] = &[
        FieldDecl::integer("source"),
        FieldDecl::integer("total")
            .generated("source + 1")
            .input_adapted(true),
    ];
    let decl = Box::leak(Box::new(
        ModelDecl::new("input_owner", "input.sample").fields(FIELDS),
    ));
    let registry = RegistryBuilder::new([&BASE, &ADDON])
        .linked(false)
        .with_decls([&*decl])
        .with_generated_input_adapters([&ADAPTER])
        .build()
        .unwrap();
    let field = registry
        .model("input.sample")
        .unwrap()
        .field("total")
        .unwrap();
    assert!(field.is_input_adapted());
    assert!(field.is_generated());
    assert!(!field.is_assignment_eligible());
    assert!(field.readonly);
}

#[test]
fn explicit_false_flag_on_ordinary_field_is_refused() {
    refuses(
        build(
            vec![FieldDecl::integer("source").input_adapted(false)],
            None,
        ),
        "Invalid generated input field 'input.sample.source': input_adapted requires a generated field",
    );
}

#[test]
fn owned_declaration_preserves_input_flag() {
    use erp_registry::{FieldDef, GeneratedInputAdapterDecl, KindDef, ModelDef};
    static ADAPTER: GeneratedInputAdapterDecl =
        GeneratedInputAdapterDecl::new("input_owner", "input.owned");
    let mut model = ModelDef::new("input_owner", "input.owned");
    model.fields.push(FieldDef::new("source", KindDef::Integer));
    model.fields.push(
        FieldDef::new("total", KindDef::Integer)
            .generated("source + 1")
            .input_adapted(true),
    );
    let registry = RegistryBuilder::new([&BASE, &ADDON])
        .linked(false)
        .with_decls([model.to_decl().unwrap()])
        .with_generated_input_adapters([&ADAPTER])
        .build()
        .unwrap();
    assert!(
        registry
            .model("input.owned")
            .unwrap()
            .field("total")
            .unwrap()
            .is_input_adapted()
    );
}
