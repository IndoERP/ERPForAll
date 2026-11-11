//! Final HTML policy metadata follows the same native and owned composition path.
use erp_fields::ErrorKind;
use erp_fields::html::{DefaultPolicy, FieldPolicy, SanitizePolicy};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "html_policy",
    depends: &["base"],
    exact_only: false,
};
fn build(field: FieldDecl) -> erp_fields::Result<erp_registry::Registry> {
    let fields = Box::leak(vec![field].into_boxed_slice());
    let model = Box::leak(Box::new(
        ModelDecl::new("html_policy", "html_policy.item").fields(fields),
    ));
    RegistryBuilder::new([&ADDON]).with_decls([&*model]).build()
}

#[test]
fn default_html_policy_is_resolved_by_registry() {
    let registry = build(FieldDecl::html("body")).unwrap();
    let model = registry.model("html_policy.item").unwrap();
    assert_eq!(
        model.field("body").unwrap().html_policy,
        Some(FieldPolicy::Sanitize(SanitizePolicy::Default(
            DefaultPolicy::new()
        )))
    );
}

#[test]
fn raw_policy_is_explicit_and_not_trusted() {
    let registry =
        build(FieldDecl::html("body").html_policy(FieldPolicy::UnsanitisedText)).unwrap();
    let model = registry.model("html_policy.item").unwrap();
    assert_eq!(
        model.field("body").unwrap().html_policy,
        Some(FieldPolicy::UnsanitisedText)
    );
}

#[test]
fn html_policy_on_non_html_field_is_refused() {
    let error =
        build(FieldDecl::text("body").html_policy(FieldPolicy::UnsanitisedText)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "field 'html_policy.item.body': HTML policy requires an Html field"
    );
}

#[test]
fn owned_policy_survives_conversion() {
    use erp_registry::{FieldDef, KindDef, ModelDef};
    let def = ModelDef::new("html_policy", "html_policy.item")
        .field(FieldDef::new("body", KindDef::Html).sanitize(false));
    let decl = def.to_decl().unwrap();
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([decl])
        .build()
        .unwrap();
    assert_eq!(
        registry
            .model("html_policy.item")
            .unwrap()
            .field("body")
            .unwrap()
            .html_policy,
        Some(FieldPolicy::UnsanitisedText)
    );
}

#[test]
fn final_inherited_policy_is_preserved_and_can_be_stricter() {
    static BASE: ModelDecl = ModelDecl::new("html_policy", "html_policy.item")
        .fields(&[FieldDecl::html("body").sanitize(false)]);
    static EXT: ModelDecl =
        ModelDecl::extension("html_policy", "html_policy.item", &["html_policy.item"]).fields(&[
            FieldDecl::html("body").html_policy(FieldPolicy::Sanitize(SanitizePolicy::Default(
                DefaultPolicy::new().with_strip_classes(),
            ))),
        ]);
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&BASE, &EXT])
        .build()
        .unwrap();
    assert_eq!(
        registry
            .model("html_policy.item")
            .unwrap()
            .field("body")
            .unwrap()
            .html_policy,
        Some(FieldPolicy::Sanitize(SanitizePolicy::Default(
            DefaultPolicy::new().with_strip_classes()
        )))
    );
}
