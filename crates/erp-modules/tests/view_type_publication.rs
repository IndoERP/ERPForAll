//! H18 view-type publication through the real ORM builder and rebuild, without a database
//! (plan WP3b). The pool is created but never connected.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_modules::{ViewTypeInputs, view_type_factory};
use erp_orm::{Context, Orm, SUPERUSER_ID, StaticPolicy};
use erp_registry::{AddonDescriptor, ModelDecl, Registry, RegistryBuilder, WEB_ADDON};
use erp_views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    InstalledClosure, MetadataFieldSet, PublishedViewTypes, SchemaNode, SchemaProperty, ViewResult,
    ViewSchema, ViewTypeContribution, ViewTypeDecl, ViewTypeDeclFn,
};
use std::sync::Arc;

static EXAMPLE: AddonDescriptor = AddonDescriptor {
    name: "example",
    depends: &["web"],
    exact_only: false,
};
static FAKE_EXAMPLE: AddonDescriptor = AddonDescriptor {
    name: "example",
    depends: &["web"],
    exact_only: false,
};
static ITEM: ModelDecl = ModelDecl::new("example", "example.item");
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
    methods: &'static [&'static str],
) -> ViewResult<ViewTypeDecl> {
    ViewTypeDecl::builder(addon, key)
        .wire_major(1)
        .renderer("r")
        .schema(&ViewSchema::new(1, &ROOT)?)
        .validator(validate)
        .builder(build)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(methods)
        .finish()
}
fn web_list() -> ViewResult<ViewTypeDecl> {
    declare("web", "list", &[])
}
fn timeline_read() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", &["read"])
}
fn timeline_unknown() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", &["unknown_method"])
}
static WEB_TYPES: [ViewTypeDeclFn; 1] = [web_list];
static WEB_TWICE: [ViewTypeDeclFn; 2] = [web_list, web_list];
static GOOD: [ViewTypeDeclFn; 1] = [timeline_read];
static BAD: [ViewTypeDeclFn; 1] = [timeline_unknown];

fn registry() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&EXAMPLE])
            .with_decls([&ITEM])
            .build()
            .unwrap(),
    )
}
fn closure_of(registry: &Registry) -> Vec<(&'static str, &'static [&'static str])> {
    registry
        .addons()
        .iter()
        .map(|addon| (addon.name, addon.depends))
        .collect()
}
fn full_closure(registry: &Registry) -> InstalledClosure {
    InstalledClosure::new(&closure_of(registry)).unwrap()
}
fn builder(registry: Arc<Registry>) -> erp_orm::OrmBuilder {
    Orm::builder(
        registry,
        erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap(),
    )
    .policy(StaticPolicy::SuperuserOnly)
}
fn contributions(example: &'static [ViewTypeDeclFn]) -> Vec<ViewTypeContribution> {
    vec![
        ViewTypeContribution::new(&WEB_ADDON, &WEB_TYPES),
        ViewTypeContribution::new(&EXAMPLE, example),
    ]
}
fn factory(
    registry: &Arc<Registry>,
    contributions: Vec<ViewTypeContribution>,
) -> erp_orm::GenerationAttachmentFactory<PublishedViewTypes, ViewTypeInputs> {
    view_type_factory(registry, contributions, full_closure(registry))
}
fn published(orm: &Orm) -> Arc<PublishedViewTypes> {
    orm.env(SUPERUSER_ID, Context::new())
        .generation_attachment::<PublishedViewTypes>()
        .unwrap()
}
fn refusal(result: Result<Orm, erp_fields::Error>) -> erp_fields::Error {
    match result {
        Ok(_) => panic!("a refused composition published an ORM"),
        Err(error) => error,
    }
}

/// registry, through the generation factory)
#[test]
fn root_and_stage_generations_publish_tables_bound_to_their_own_registry() {
    let root_registry = registry();
    let root = builder(root_registry.clone())
        .generation_attachment_with(factory(&root_registry, contributions(&GOOD)))
        .build()
        .unwrap();
    let root_table = published(&root);
    root_table.require_registry(&root_registry).unwrap();
    assert!(root_table.find("example.timeline").is_some());

    let stage_registry = registry();
    let stage = root
        .rebuild(stage_registry.clone())
        .generation_attachment_with(factory(&stage_registry, contributions(&GOOD)))
        .build()
        .unwrap();
    let stage_table = published(&stage);
    stage_table.require_registry(&stage_registry).unwrap();
    assert!(!Arc::ptr_eq(&root_table, &stage_table));
    // The stage table refuses the root registry although the contents are equal.
    assert!(stage_table.require_registry(&root_registry).is_err());
    // The root generation is untouched.
    assert!(Arc::ptr_eq(&root_table, &published(&root)));
}

#[test]
fn a_factory_prepared_for_the_old_registry_is_refused_on_rebuild() {
    let old = registry();
    let root = builder(old.clone())
        .generation_attachment_with(factory(&old, contributions(&GOOD)))
        .build()
        .unwrap();
    let new = registry();
    let error = refusal(
        root.rebuild(new)
            .generation_attachment_with(factory(&old, contributions(&GOOD)))
            .build(),
    );
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "generation attachment registry identity does not match the ORM; rebuild and revalidate the composition product"
    );
}

/// previous generation serving)
#[test]
fn a_failed_composition_does_not_publish_a_candidate() {
    let old = registry();
    let root = builder(old.clone())
        .generation_attachment_with(factory(&old, contributions(&GOOD)))
        .build()
        .unwrap();
    let before = published(&root);
    let new = registry();
    let error = refusal(
        root.rebuild(new.clone())
            .generation_attachment_with(factory(&new, contributions(&BAD)))
            .build(),
    );
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "View-type publication refused: View-type declaration binds the unknown data method \"unknown_method\" (view type \"example.timeline\" of addon \"example\")"
    );
    assert!(Arc::ptr_eq(&before, &published(&root)));
}

#[test]
fn equal_names_never_substitute_the_installed_descriptor_identity() {
    let registry = registry();
    let error = refusal(
        builder(registry.clone())
            .generation_attachment_with(factory(
                &registry,
                vec![
                    ViewTypeContribution::new(&WEB_ADDON, &WEB_TYPES),
                    ViewTypeContribution::new(&FAKE_EXAMPLE, &GOOD),
                ],
            ))
            .build(),
    );
    assert_eq!(
        error.message(),
        "View-type publication refused: View-type contributing owner descriptor differs from the actual installed descriptor"
    );
}

#[test]
fn a_declaration_from_a_different_owner_than_its_contributor_is_refused() {
    let registry = registry();
    let error = refusal(
        builder(registry.clone())
            .generation_attachment_with(factory(
                &registry,
                vec![ViewTypeContribution::new(&EXAMPLE, &WEB_TYPES)],
            ))
            .build(),
    );
    assert_eq!(
        error.message(),
        "View-type publication refused: View-type declared owner differs from its contributing descriptor"
    );
}

#[test]
fn a_duplicate_key_refuses_the_generation() {
    let registry = registry();
    let error = refusal(
        builder(registry.clone())
            .generation_attachment_with(factory(
                &registry,
                vec![ViewTypeContribution::new(&WEB_ADDON, &WEB_TWICE)],
            ))
            .build(),
    );
    assert_eq!(
        error.message(),
        "View-type publication refused: View-type declaration registers a duplicate key (view type \"list\" of addon \"web\")"
    );
}

#[test]
fn an_installed_closure_that_differs_from_the_registry_is_refused() {
    let registry = registry();
    // `example` is in the registry but absent from the installed set.
    let without_example: Vec<_> = closure_of(&registry)
        .into_iter()
        .filter(|(name, _)| *name != "example")
        .collect();
    let error = refusal(
        builder(registry.clone())
            .generation_attachment_with(view_type_factory(
                &registry,
                contributions(&GOOD)[..1].to_vec(),
                InstalledClosure::new(&without_example).unwrap(),
            ))
            .build(),
    );
    assert_eq!(
        error.message(),
        "View-type publication refused: Registry addon \"example\" is not in the installed dependency closure"
    );
    // The closure names an addon the registry does not contain.
    let mut with_ghost = closure_of(&registry);
    with_ghost.push(("ghost", &["base"]));
    let error = refusal(
        builder(registry.clone())
            .generation_attachment_with(view_type_factory(
                &registry,
                contributions(&GOOD),
                InstalledClosure::new(&with_ghost).unwrap(),
            ))
            .build(),
    );
    assert_eq!(
        error.message(),
        "View-type publication refused: Installed addon \"ghost\" is not in the registry"
    );
}

/// not published, and does not fail the generation)
#[test]
fn contributions_of_addons_that_are_not_installed_are_left_out() {
    let registry = Arc::new(
        RegistryBuilder::new(Vec::<&'static AddonDescriptor>::new())
            .build()
            .unwrap(),
    );
    let orm = builder(registry.clone())
        .generation_attachment_with(factory(&registry, contributions(&GOOD)))
        .build()
        .unwrap();
    let table = published(&orm);
    assert!(table.find("list").is_some());
    assert!(table.find("example.timeline").is_none());
}

static NATIVE_WEB: AddonDescriptor = AddonDescriptor {
    name: "web",
    depends: &["base"],
    exact_only: false,
};

/// registry placeholder, owns the contribution at the module system's actual root)
#[tokio::test]
async fn native_framework_owner_is_retained_at_the_module_system_root() {
    let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap();
    let system = erp_modules::ModuleSystem::builder(pool, erp_modules::AddonIndex::new(true))
        .linked_hooks(false)
        .linked_migrations(false)
        .code([&NATIVE_WEB])
        .view_types([ViewTypeContribution::new(&NATIVE_WEB, &WEB_TYPES)])
        .build()
        .unwrap();
    let env = system.system_env(Context::new());
    let table = env.generation_attachment::<PublishedViewTypes>().unwrap();
    table.require_registry(env.registry()).unwrap();
    assert!(std::ptr::eq(table.owner("list").unwrap(), &NATIVE_WEB));
}

/// a generation that does not contain that addon)
#[tokio::test]
async fn linked_but_uninstalled_addon_contributes_no_view_types() {
    let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap();
    let system = erp_modules::ModuleSystem::builder(pool, erp_modules::AddonIndex::new(true))
        .linked_hooks(false)
        .linked_migrations(false)
        .code([&EXAMPLE])
        .view_types([ViewTypeContribution::new(&EXAMPLE, &GOOD)])
        .build()
        .unwrap();
    let env = system.system_env(Context::new());
    let table = env.generation_attachment::<PublishedViewTypes>().unwrap();
    assert!(table.find("example.timeline").is_none());
}

static OTHER: AddonDescriptor = AddonDescriptor {
    name: "other",
    depends: &["example"],
    exact_only: false,
};
static OTHER_EXTENSION: ModelDecl =
    ModelDecl::extension("other", "example.item", &["example.item"]);
fn example_with_foreign_method() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", &["other_read"])
}
static FOREIGN_METHOD: [ViewTypeDeclFn; 1] = [example_with_foreign_method];
fn unused_bind<'a>(
    _: &'a mut erp_orm::Transaction,
    _: erp_orm::Records,
    _: erp_orm::rpc::RpcArgs,
) -> erp_orm::BoxFuture<'a, Result<erp_fields::Value, erp_orm::rpc::BindError>> {
    Box::pin(async { Ok(erp_fields::Value::False) })
}
static OTHER_RPC: erp_orm::rpc::RpcImpl = erp_orm::rpc::RpcImpl {
    addon: "other",
    model: "example.item",
    method: "other_read",
    decl: erp_orm::rpc::RpcDecl::Expose {
        effect: erp_orm::rpc::Effect::Read,
        receiver: erp_orm::rpc::Receiver::Model,
        params: &[],
        bind: unused_bind,
    },
    file: file!(),
    line: line!(),
};

/// satisfy a declaration merely because that addon extends a model in the declaration's closure)
#[test]
fn data_method_exposure_must_belong_to_the_declaring_addons_dependency_closure() {
    let registry = Arc::new(
        RegistryBuilder::new([&EXAMPLE, &OTHER])
            .with_decls([&ITEM, &OTHER_EXTENSION])
            .build()
            .unwrap(),
    );
    let error = refusal(
        builder(registry.clone())
            .rpc_impls([&OTHER_RPC])
            .generation_attachment_with(factory(&registry, contributions(&FOREIGN_METHOD)))
            .build(),
    );
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "View-type publication refused: View-type declaration binds the unknown data method \"other_read\" (view type \"example.timeline\" of addon \"example\")"
    );
}
