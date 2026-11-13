//! Read-audit model declarations.
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::OnDelete;
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, ModelDef, ModelKind, Registry, RegistryBuilder,
};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "audit_reads_test",
    depends: &["base"],
    exact_only: false,
};

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls(decls.into_iter().map(|decl| &*Box::leak(Box::new(decl))))
        .build()
}

#[test]
fn opt_in_is_default_false_and_monotonic_across_extensions() {
    let plain = build(vec![ModelDecl::new("audit_reads_test", "test.item")]).unwrap();
    assert!(!plain.model("test.item").unwrap().audit_reads);
    let enabled = build(vec![
        ModelDecl::new("audit_reads_test", "test.item"),
        ModelDecl::extension("audit_reads_test", "test.item", &["test.item"]).audit_reads(true),
        ModelDecl::extension("audit_reads_test", "test.item", &["test.item"]).audit_reads(false),
    ])
    .unwrap();
    assert!(enabled.model("test.item").unwrap().audit_reads);
    let owner_enabled = build(vec![
        ModelDecl::new("audit_reads_test", "test.item").audit_reads(true),
        ModelDecl::extension("audit_reads_test", "test.item", &["test.item"]).audit_reads(false),
    ])
    .unwrap();
    assert!(owner_enabled.model("test.item").unwrap().audit_reads);
    // A rebuild without the opting-in extension does not inherit its old registry state.
    let rebuilt = build(vec![ModelDecl::new("audit_reads_test", "test.item")]).unwrap();
    assert!(!rebuilt.model("test.item").unwrap().audit_reads);
}

#[test]
fn owned_declarations_preserve_audit_reads() {
    let owner = ModelDef::new("audit_reads_test", "test.owned").audit_reads(true);
    let decl = owner.to_decl().unwrap();
    assert!(decl.audit_reads);
    let disabled = ModelDef::new("audit_reads_test", "test.owned")
        .to_decl()
        .unwrap();
    assert!(!disabled.audit_reads);
    assert!(
        !std::ptr::eq(decl, disabled),
        "interning must retain the flag"
    );
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([decl])
        .build()
        .unwrap();
    assert!(registry.model("test.owned").unwrap().audit_reads);
    assert!(
        !ModelDef::new("audit_reads_test", "test.plain")
            .to_decl()
            .unwrap()
            .audit_reads
    );
}

#[test]
fn classical_inheritance_composes_parent_audit_reads_opt_in() {
    let registry = build(vec![
        ModelDecl::new("audit_reads_test", "test.parent").audit_reads(true),
        ModelDecl::new("audit_reads_test", "test.child")
            .inherit(&["test.parent"])
            .audit_reads(false),
        ModelDecl::new("audit_reads_test", "test.unrelated"),
    ])
    .unwrap();
    assert!(registry.model("test.parent").unwrap().audit_reads);
    assert!(registry.model("test.child").unwrap().audit_reads);
    assert!(!registry.model("test.unrelated").unwrap().audit_reads);
}

#[test]
fn delegation_keeps_parent_and_child_opt_in_independent() {
    const TO_AUDITED: &[FieldDecl] = &[FieldDecl::many2one("parent_id", "test.parent_audit_reads")
        .required(true)
        .ondelete(OnDelete::Cascade)];
    const TO_PLAIN: &[FieldDecl] = &[FieldDecl::many2one("parent_id", "test.parent_plain")
        .required(true)
        .ondelete(OnDelete::Cascade)];
    const PARENT_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];
    let registry = build(vec![
        ModelDecl::new("audit_reads_test", "test.parent_audit_reads")
            .fields(PARENT_FIELDS)
            .audit_reads(true),
        ModelDecl::new("audit_reads_test", "test.parent_plain").fields(PARENT_FIELDS),
        ModelDecl::new("audit_reads_test", "test.delegated_plain")
            .inherits(&[("test.parent_audit_reads", "parent_id")])
            .fields(TO_AUDITED),
        ModelDecl::new("audit_reads_test", "test.delegated_audit_reads")
            .inherits(&[("test.parent_plain", "parent_id")])
            .fields(TO_PLAIN)
            .audit_reads(true),
    ])
    .unwrap();
    assert!(
        registry
            .model("test.parent_audit_reads")
            .unwrap()
            .audit_reads
    );
    assert!(!registry.model("test.delegated_plain").unwrap().audit_reads);
    assert!(!registry.model("test.parent_plain").unwrap().audit_reads);
    assert!(
        registry
            .model("test.delegated_audit_reads")
            .unwrap()
            .audit_reads
    );
    // These are genuine delegated fields, not a fixture with an unused parent reference.
    assert!(
        registry
            .model("test.delegated_plain")
            .unwrap()
            .field("name")
            .is_some()
    );
    assert!(
        registry
            .model("test.delegated_audit_reads")
            .unwrap()
            .field("name")
            .is_some()
    );
}

#[test]
fn audit_reads_extensions_compose_in_either_boolean_order() {
    for (first, second) in [(true, false), (false, true)] {
        let registry = build(vec![
            ModelDecl::new("audit_reads_test", "test.ordered"),
            ModelDecl::extension("audit_reads_test", "test.ordered", &["test.ordered"])
                .audit_reads(first),
            ModelDecl::extension("audit_reads_test", "test.ordered", &["test.ordered"])
                .audit_reads(second),
        ])
        .unwrap();
        assert!(
            registry.model("test.ordered").unwrap().audit_reads,
            "OR must hold for extension order {first}/{second}"
        );
    }
}

#[test]
fn abstract_and_transient_models_can_opt_in() {
    let registry = build(vec![
        ModelDecl::new("audit_reads_test", "test.mixin")
            .kind(ModelKind::Abstract)
            .audit_reads(true),
        ModelDecl::new("audit_reads_test", "test.concrete").inherit(&["test.mixin"]),
        ModelDecl::new("audit_reads_test", "test.wizard")
            .kind(ModelKind::Transient)
            .audit_reads(true),
    ])
    .unwrap();
    assert!(registry.model("test.concrete").unwrap().audit_reads);
    assert!(registry.model("test.wizard").unwrap().audit_reads);
}
