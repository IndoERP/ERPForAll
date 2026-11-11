//! Transient cleanup policy metadata (package A). Pure: no
//! database. Declaring the policy never starts a cleanup; these tests cover the metadata only.
#![allow(clippy::unwrap_used, reason = "tests")]

use erp_registry::{
    AddonDescriptor, DEFAULT_TRANSIENT_MAX_AGE_SECONDS, MAX_TRANSIENT_MAX_AGE_SECONDS,
    MAX_TRANSIENT_MAX_COUNT, ModelDecl, ModelDef, ModelKind, Overlay, Registry, RegistryBuilder,
    TransientPolicy,
};

static OTHER: AddonDescriptor = AddonDescriptor {
    name: "other",
    depends: &["base"],
    exact_only: false,
};
static LATER: AddonDescriptor = AddonDescriptor {
    name: "later",
    depends: &["other"],
    exact_only: false,
};

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&OTHER, &LATER])
        .with_decls(decls)
        .build()
}

fn build_with_defs(decls: Vec<ModelDecl>, defs: Vec<ModelDef>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs)?;
    RegistryBuilder::new([&OTHER, &LATER])
        .with_decls(decls)
        .overlay(&overlay)
        .build()
}

fn policy(registry: &Registry, model: &str) -> Option<TransientPolicy> {
    registry.model(model).unwrap().transient_policy
}

fn transient(name: &'static str) -> ModelDecl {
    ModelDecl::new("other", name).kind(ModelKind::Transient)
}

#[test]
fn defaults_are_one_hour_and_no_count_limit() {
    let registry = build(vec![transient("x.wizard")]).unwrap();
    let p = policy(&registry, "x.wizard").unwrap();
    assert_eq!(p.max_age_seconds(), Some(DEFAULT_TRANSIENT_MAX_AGE_SECONDS));
    assert_eq!(DEFAULT_TRANSIENT_MAX_AGE_SECONDS, 3_600);
    assert_eq!(p.max_count(), None);
    assert!(!p.is_cleanup_disabled());
    assert_eq!(p, TransientPolicy::default());
}

#[test]
fn regular_and_abstract_models_have_no_policy() {
    let registry = build(vec![
        ModelDecl::new("other", "x.regular"),
        ModelDecl::new("other", "x.abstract").kind(ModelKind::Abstract),
    ])
    .unwrap();
    assert_eq!(policy(&registry, "x.regular"), None);
    assert_eq!(policy(&registry, "x.abstract"), None);
}

#[test]
fn declared_values_are_kept_and_zero_disables_a_threshold() {
    let registry = build(vec![
        transient("x.a")
            .transient_max_age_seconds(7_200)
            .transient_max_count(50),
        transient("x.no_age").transient_max_age_seconds(0),
        transient("x.no_count").transient_max_count(0),
        transient("x.neither")
            .transient_max_age_seconds(0)
            .transient_max_count(0),
    ])
    .unwrap();
    let a = policy(&registry, "x.a").unwrap();
    assert_eq!(
        (a.max_age_seconds(), a.max_count()),
        (Some(7_200), Some(50))
    );
    let no_age = policy(&registry, "x.no_age").unwrap();
    assert_eq!((no_age.max_age_seconds(), no_age.max_count()), (None, None));
    assert!(no_age.is_cleanup_disabled());
    // zero count is the explicit spelling of "no count limit": the default age stays
    let no_count = policy(&registry, "x.no_count").unwrap();
    assert_eq!(
        (no_count.max_age_seconds(), no_count.max_count()),
        (Some(3_600), None)
    );
    let neither = policy(&registry, "x.neither").unwrap();
    assert!(neither.is_cleanup_disabled());
}

#[test]
fn bounds_are_inclusive_and_overflow_is_refused_by_name() {
    let ok = build(vec![
        transient("x.max")
            .transient_max_age_seconds(MAX_TRANSIENT_MAX_AGE_SECONDS)
            .transient_max_count(MAX_TRANSIENT_MAX_COUNT),
    ])
    .unwrap();
    let p = policy(&ok, "x.max").unwrap();
    assert_eq!(p.max_age_seconds(), Some(315_360_000));
    assert_eq!(p.max_count(), Some(9_223_372_036_854_775_807));

    let err = build(vec![
        transient("x.old").transient_max_age_seconds(MAX_TRANSIENT_MAX_AGE_SECONDS + 1),
    ])
    .unwrap_err();
    assert!(err.message().contains("x.old"), "{err}");
    assert!(err.message().contains("transient_max_age_seconds"), "{err}");
    assert!(err.message().contains("315360000"), "{err}");

    let err = build(vec![
        transient("x.many").transient_max_count(MAX_TRANSIENT_MAX_COUNT + 1),
    ])
    .unwrap_err();
    assert!(err.message().contains("x.many"), "{err}");
    assert!(err.message().contains("transient_max_count"), "{err}");
}

#[test]
fn only_transient_models_may_declare_a_policy() {
    for kind in [ModelKind::Regular, ModelKind::Abstract] {
        for (attr, decl) in [
            (
                "transient_max_age_seconds",
                ModelDecl::new("other", "x.not_transient")
                    .kind(kind)
                    .transient_max_age_seconds(60),
            ),
            (
                "transient_max_count",
                ModelDecl::new("other", "x.not_transient")
                    .kind(kind)
                    .transient_max_count(5),
            ),
        ] {
            let err = build(vec![decl]).unwrap_err();
            assert!(err.message().contains("x.not_transient"), "{err}");
            assert!(err.message().contains(attr), "{err}");
            assert!(err.message().contains("not a transient model"), "{err}");
        }
    }
    // even a disabling zero is a declaration
    let err = build(vec![
        ModelDecl::new("other", "x.zero").transient_max_count(0),
    ])
    .unwrap_err();
    assert!(err.message().contains("transient_max_count"), "{err}");
}

#[test]
fn extensions_inherit_and_explicit_attributes_override_in_composition_order() {
    let registry = build(vec![
        transient("x.wizard")
            .transient_max_age_seconds(120)
            .transient_max_count(5),
        // inherits both
        ModelDecl::extension("later", "x.wizard", &["x.wizard"]).kind(ModelKind::Transient),
    ])
    .unwrap();
    let p = policy(&registry, "x.wizard").unwrap();
    assert_eq!((p.max_age_seconds(), p.max_count()), (Some(120), Some(5)));

    let registry = build(vec![
        transient("x.wizard")
            .transient_max_age_seconds(120)
            .transient_max_count(5),
        // overrides only the count; zero on the age disables it explicitly
        ModelDecl::extension("later", "x.wizard", &["x.wizard"])
            .kind(ModelKind::Transient)
            .transient_max_count(10)
            .transient_max_age_seconds(0),
    ])
    .unwrap();
    let p = policy(&registry, "x.wizard").unwrap();
    assert_eq!((p.max_age_seconds(), p.max_count()), (None, Some(10)));

    // an extension of a regular model cannot add one
    let err = build(vec![
        ModelDecl::new("other", "x.plain"),
        ModelDecl::extension("later", "x.plain", &["x.plain"]).transient_max_count(3),
    ])
    .unwrap_err();
    assert!(err.message().contains("x.plain"), "{err}");
    assert!(err.message().contains("transient_max_count"), "{err}");
}

#[test]
fn direct_declaration_and_owned_definition_compose_the_same_policy() {
    let native = build(vec![
        transient("x.wizard")
            .transient_max_age_seconds(900)
            .transient_max_count(0),
    ])
    .unwrap();

    let mut def = ModelDef::new("other", "x.wizard")
        .transient_max_age_seconds(900)
        .transient_max_count(0);
    def.kind = Some(ModelKind::Transient);
    let owned = build_with_defs(vec![], vec![def]).unwrap();
    assert_eq!(policy(&native, "x.wizard"), policy(&owned, "x.wizard"));
    assert_eq!(
        policy(&owned, "x.wizard").unwrap().max_age_seconds(),
        Some(900)
    );

    // an owned extension composes on top of a native declaration, kind taken from the model
    let ext = ModelDef::extension("later", "x.wizard").transient_max_count(7);
    let composed = build_with_defs(
        vec![transient("x.wizard").transient_max_age_seconds(900)],
        vec![ext],
    )
    .unwrap();
    let p = policy(&composed, "x.wizard").unwrap();
    assert_eq!((p.max_age_seconds(), p.max_count()), (Some(900), Some(7)));

    // an owned definition of a regular model is refused by name
    let bad = ModelDef::new("other", "x.plain").transient_max_age_seconds(5);
    let err = build_with_defs(vec![], vec![bad]).unwrap_err();
    assert!(err.message().contains("x.plain"), "{err}");
    assert!(err.message().contains("transient_max_age_seconds"), "{err}");

    // and an owned definition over the bound
    let mut big = ModelDef::new("other", "x.big")
        .transient_max_age_seconds(MAX_TRANSIENT_MAX_AGE_SECONDS + 1);
    big.kind = Some(ModelKind::Transient);
    let err = build_with_defs(vec![], vec![big]).unwrap_err();
    assert!(err.message().contains("transient_max_age_seconds"), "{err}");
}
