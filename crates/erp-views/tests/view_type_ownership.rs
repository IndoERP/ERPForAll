//! Composition binds declarations to installed descriptors, never owner strings.
use erp_registry::{AddonDescriptor, BASE_ADDON, RegistryBuilder, WEB_ADDON};
use erp_views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    MetadataFieldSet, OwnerBoundViewTypes, SchemaNode, SchemaProperty, ViewResult, ViewSchema,
    ViewTypeContribution, ViewTypeDecl, ViewTypeDeclFn,
};
use std::sync::Arc;

static CUSTOM: AddonDescriptor = AddonDescriptor {
    name: "example",
    depends: &["web"],
    exact_only: false,
};
static FAKE_WEB: AddonDescriptor = AddonDescriptor {
    name: "web",
    depends: &["base"],
    exact_only: false,
};
static FAKE_CUSTOM: AddonDescriptor = AddonDescriptor {
    name: "example",
    depends: &["web"],
    exact_only: false,
};
static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static ROOT: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("label", &TEXT)]);

fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
    budget.check()
}
fn build(
    _: &CheckedViewInput<'_>,
    root: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    root.object(|object| object.set("label", |value| value.text("x")))
}
fn declare(
    addon: &'static str,
    key: &'static str,
    renderer: &'static str,
) -> ViewResult<ViewTypeDecl> {
    ViewTypeDecl::builder(addon, key)
        .wire_major(1)
        .renderer(renderer)
        .schema(&ViewSchema::new(1, &ROOT)?)
        .validator(validate)
        .builder(build)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(&[])
        .finish()
}
fn web_list() -> ViewResult<ViewTypeDecl> {
    declare("web", "list", "list")
}
fn example_timeline() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", "timeline")
}
static WEB_TYPES: [ViewTypeDeclFn; 1] = [web_list];
static CUSTOM_TYPES: [ViewTypeDeclFn; 1] = [example_timeline];
static BASE_TYPES: [ViewTypeDeclFn; 0] = [];

#[test]
fn canonical_builtin_and_installed_native_owners_bind_to_the_exact_registry() {
    let registry = Arc::new(RegistryBuilder::new([&CUSTOM]).build().unwrap());
    let table = OwnerBoundViewTypes::bind(
        registry.clone(),
        &[
            ViewTypeContribution::new(&BASE_ADDON, &BASE_TYPES),
            ViewTypeContribution::new(&WEB_ADDON, &WEB_TYPES),
            ViewTypeContribution::new(&CUSTOM, &CUSTOM_TYPES),
        ],
    )
    .unwrap();
    table.require_registry(&registry).unwrap();
    let other = Arc::new(RegistryBuilder::new([&CUSTOM]).build().unwrap());
    assert!(table.require_registry(&other).is_err());
}

#[test]
fn equal_names_never_replace_actual_installed_owner_identities() {
    let registry = Arc::new(RegistryBuilder::new([&CUSTOM]).build().unwrap());
    for contribution in [
        ViewTypeContribution::new(&FAKE_WEB, &WEB_TYPES),
        ViewTypeContribution::new(&FAKE_CUSTOM, &CUSTOM_TYPES),
    ] {
        let error = OwnerBoundViewTypes::bind(registry.clone(), &[contribution]).unwrap_err();
        assert!(error.to_string().contains("contributing owner descriptor"));
    }
    let error =
        OwnerBoundViewTypes::bind(registry, &[ViewTypeContribution::new(&CUSTOM, &WEB_TYPES)])
            .unwrap_err();
    assert!(error.to_string().contains("declared owner"));
    let absent = Arc::new(
        RegistryBuilder::new(Vec::<&'static AddonDescriptor>::new())
            .build()
            .unwrap(),
    );
    let error =
        OwnerBoundViewTypes::bind(absent, &[ViewTypeContribution::new(&CUSTOM, &CUSTOM_TYPES)])
            .unwrap_err();
    assert!(error.to_string().contains("not installed"));
}
