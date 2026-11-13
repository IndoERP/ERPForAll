//! Eventful model declarations (S6).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, ModelDef, ModelKind, Registry, RegistryBuilder,
};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "eventful_test",
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
    let plain = build(vec![ModelDecl::new("eventful_test", "test.item")]).unwrap();
    assert!(!plain.model("test.item").unwrap().eventful);
    let enabled = build(vec![
        ModelDecl::new("eventful_test", "test.item"),
        ModelDecl::extension("eventful_test", "test.item", &["test.item"]).eventful(true),
        ModelDecl::extension("eventful_test", "test.item", &["test.item"]).eventful(false),
    ])
    .unwrap();
    assert!(enabled.model("test.item").unwrap().eventful);
    let owner_enabled = build(vec![
        ModelDecl::new("eventful_test", "test.item").eventful(true),
        ModelDecl::extension("eventful_test", "test.item", &["test.item"]).eventful(false),
    ])
    .unwrap();
    assert!(owner_enabled.model("test.item").unwrap().eventful);
    // A rebuild without the opting-in extension does not inherit its old registry state.
    let rebuilt = build(vec![ModelDecl::new("eventful_test", "test.item")]).unwrap();
    assert!(!rebuilt.model("test.item").unwrap().eventful);
}

#[test]
fn abstract_and_transient_eventful_models_are_refused() {
    for (kind, label) in [
        (ModelKind::Abstract, "abstract"),
        (ModelKind::Transient, "transient"),
    ] {
        let error = build(vec![
            ModelDecl::new("eventful_test", "test.bad")
                .kind(kind)
                .eventful(true),
        ])
        .expect_err("invalid eventful model must be refused");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!(
                "model 'test.bad' of addon 'eventful_test' cannot be eventful: {label} models do not support event capture (a later release)"
            )
        );
    }
}

#[test]
fn event_namespace_model_cannot_be_eventful() {
    let decl = Box::leak(Box::new(
        ModelDecl::new("eventful_test", "test.event").eventful(true),
    ));
    let error = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_core_event_decls([&*decl])
        .build()
        .expect_err("recursive capture must be refused");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "model 'test.event' of addon 'eventful_test' cannot be eventful: the closed event namespace does not support event capture (a later release)"
    );
}

#[test]
fn owned_declarations_preserve_eventful() {
    let owner = ModelDef::new("eventful_test", "test.owned").eventful(true);
    let decl = owner.to_decl().unwrap();
    assert!(decl.eventful);
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([decl])
        .build()
        .unwrap();
    assert!(registry.model("test.owned").unwrap().eventful);
    assert!(
        !ModelDef::new("eventful_test", "test.plain")
            .to_decl()
            .unwrap()
            .eventful
    );
}

#[test]
fn invalid_extension_reports_the_opt_in_addon() {
    static EXT: AddonDescriptor = AddonDescriptor {
        name: "eventful_ext",
        depends: &["eventful_test"],
        exact_only: false,
    };
    let decls = vec![
        ModelDecl::new("eventful_test", "test.wizard").kind(ModelKind::Transient),
        ModelDecl::extension("eventful_ext", "test.wizard", &["test.wizard"])
            .kind(ModelKind::Transient)
            .eventful(true),
    ];
    let error = RegistryBuilder::new([&ADDON, &EXT])
        .linked(false)
        .with_decls(decls.into_iter().map(|decl| &*Box::leak(Box::new(decl))))
        .build()
        .expect_err("an extension cannot make transient capture valid");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "model 'test.wizard' of addon 'eventful_ext' cannot be eventful: transient models do not support event capture (a later release)"
    );
}

#[test]
fn classical_inheritance_composes_parent_eventful_opt_in() {
    let registry = build(vec![
        ModelDecl::new("eventful_test", "test.parent").eventful(true),
        ModelDecl::new("eventful_test", "test.child")
            .inherit(&["test.parent"])
            .eventful(false),
        ModelDecl::new("eventful_test", "test.unrelated"),
    ])
    .unwrap();
    assert!(registry.model("test.parent").unwrap().eventful);
    assert!(registry.model("test.child").unwrap().eventful);
    assert!(!registry.model("test.unrelated").unwrap().eventful);
}

#[test]
fn review_delegation_keeps_parent_and_child_opt_in_independent() {
    const TO_EVENTFUL: &[FieldDecl] = &[FieldDecl::many2one("parent_id", "test.parent_eventful")
        .required(true)
        .ondelete(OnDelete::Cascade)];
    const TO_PLAIN: &[FieldDecl] = &[FieldDecl::many2one("parent_id", "test.parent_plain")
        .required(true)
        .ondelete(OnDelete::Cascade)];
    const PARENT_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];
    let registry = build(vec![
        ModelDecl::new("eventful_test", "test.parent_eventful")
            .fields(PARENT_FIELDS)
            .eventful(true),
        ModelDecl::new("eventful_test", "test.parent_plain").fields(PARENT_FIELDS),
        ModelDecl::new("eventful_test", "test.delegated_plain")
            .inherits(&[("test.parent_eventful", "parent_id")])
            .fields(TO_EVENTFUL),
        ModelDecl::new("eventful_test", "test.delegated_eventful")
            .inherits(&[("test.parent_plain", "parent_id")])
            .fields(TO_PLAIN)
            .eventful(true),
    ])
    .unwrap();
    assert!(registry.model("test.parent_eventful").unwrap().eventful);
    assert!(!registry.model("test.delegated_plain").unwrap().eventful);
    assert!(!registry.model("test.parent_plain").unwrap().eventful);
    assert!(registry.model("test.delegated_eventful").unwrap().eventful);
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
            .model("test.delegated_eventful")
            .unwrap()
            .field("name")
            .is_some()
    );
}

#[test]
fn review_eventful_extensions_compose_in_either_boolean_order() {
    for (first, second) in [(true, false), (false, true)] {
        let registry = build(vec![
            ModelDecl::new("eventful_test", "test.ordered"),
            ModelDecl::extension("eventful_test", "test.ordered", &["test.ordered"])
                .eventful(first),
            ModelDecl::extension("eventful_test", "test.ordered", &["test.ordered"])
                .eventful(second),
        ])
        .unwrap();
        assert!(
            registry.model("test.ordered").unwrap().eventful,
            "OR must hold for extension order {first}/{second}"
        );
    }
}

/// Field names `0..n`, leaked because declarations are `'static`.
fn wide_fields(n: usize) -> &'static [FieldDecl] {
    let fields: Vec<FieldDecl> = (0..n)
        .map(|i| FieldDecl::integer(Box::leak(format!("f{i}").into_boxed_str())))
        .collect();
    Box::leak(fields.into_boxed_slice())
}

/// The names one event may carry for `model`: fields with a column of their own, without `id`.
fn countable(registry: &Registry, model: &str) -> usize {
    registry
        .model(model)
        .unwrap()
        .fields
        .iter()
        .filter(|f| f.has_column() && f.inherited.is_none() && f.name != "id")
        .count()
}

/// Declared fields that make `countable` come to `total`: the registry adds its own columns
/// (log-access fields), so the baseline is measured from a model with none declared.
fn declared_for(total: usize) -> usize {
    let empty = build(vec![ModelDecl::new("eventful_test", "wide.item")]).unwrap();
    total - countable(&empty, "wide.item")
}

#[test]
fn eventful_model_with_128_columns_builds() {
    let n = declared_for(128);
    let registry = build(vec![
        ModelDecl::new("eventful_test", "wide.item")
            .fields(wide_fields(n))
            .eventful(true),
    ])
    .unwrap();
    assert_eq!(countable(&registry, "wide.item"), 128);
    assert!(registry.model("wide.item").unwrap().eventful);
}

#[test]
fn eventful_model_with_129_columns_is_refused() {
    let n = declared_for(129);
    let error = build(vec![
        ModelDecl::new("eventful_test", "wide.item")
            .fields(wide_fields(n))
            .eventful(true),
    ])
    .expect_err("129 names cannot fit in one event");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "model 'wide.item' of addon 'eventful_test' cannot be eventful: 129 fields with a column exceed the 128 field names one event may carry (a later release)"
    );
}

#[test]
fn wide_model_that_is_not_eventful_builds() {
    let n = declared_for(129);
    let registry = build(vec![
        ModelDecl::new("eventful_test", "wide.item").fields(wide_fields(n)),
    ])
    .unwrap();
    assert_eq!(countable(&registry, "wide.item"), 129);
    assert!(!registry.model("wide.item").unwrap().eventful);
}

#[test]
fn extension_making_a_wide_model_eventful_is_refused_naming_the_extension() {
    static EXT: AddonDescriptor = AddonDescriptor {
        name: "eventful_wide_ext",
        depends: &["eventful_test"],
        exact_only: false,
    };
    let n = declared_for(129);
    let decls = vec![
        ModelDecl::new("eventful_test", "wide.item").fields(wide_fields(n)),
        ModelDecl::new("eventful_wide_ext", "wide.item")
            .inherit(&["wide.item"])
            .eventful(true),
    ];
    let error = RegistryBuilder::new([&ADDON, &EXT])
        .linked(false)
        .with_decls(decls.into_iter().map(|decl| &*Box::leak(Box::new(decl))))
        .build()
        .expect_err("the extension cannot make a 129-column model eventful");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "model 'wide.item' of addon 'eventful_wide_ext' cannot be eventful: 129 fields with a column exceed the 128 field names one event may carry (a later release)"
    );
}

/// A custom (overlay) field that brings an eventful 128-column model to 129 refuses the build
/// like any other eventful refusal; the model is wrong, not one field, so no drop policy applies.
#[test]
fn overlay_field_pushing_an_eventful_model_over_the_ceiling_is_refused() {
    use erp_registry::{FieldDef, KindDef, ManualFieldPolicy, Overlay};
    let decl: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("eventful_test", "wide.item")
            .fields(wide_fields(declared_for(128)))
            .eventful(true),
    ));
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "wide.item",
            FieldDef::new("x_extra", KindDef::Char { size: None }),
        )
        .unwrap();
    let build = |policy| {
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([decl])
            .overlay(&overlay)
            .manual_fields(policy)
            .build()
    };
    for policy in [ManualFieldPolicy::Strict, ManualFieldPolicy::DropBroken] {
        let error = build(policy).expect_err("129 columns cannot be eventful");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "model 'wide.item' of addon 'eventful_test' cannot be eventful: 129 fields with a column exceed the 128 field names one event may carry (a later release)"
        );
    }
}
