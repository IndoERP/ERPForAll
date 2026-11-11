//! The module system publishes the H18 view-type table at its root ORM and at every stage ORM
//! (plan WP3b). Needs the test database cluster.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod common;
use common::*;
use erp_modules::InitOptions;
use erp_orm::{Context, Orm, SUPERUSER_ID};
use erp_registry::WEB_ADDON;
use erp_testkit::TestDb;
use erp_views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    MetadataFieldSet, PublishedViewTypes, SchemaNode, SchemaProperty, ViewResult, ViewSchema,
    ViewTypeContribution, ViewTypeDecl, ViewTypeDeclFn,
};
use std::sync::Arc;

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
fn web_type(key: &'static str) -> ViewResult<ViewTypeDecl> {
    ViewTypeDecl::builder("web", key)
        .wire_major(1)
        .renderer(key)
        .schema(&ViewSchema::new(1, &ROOT)?)
        .validator(validate)
        .builder(build)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(&[])
        .finish()
}
fn web_list() -> ViewResult<ViewTypeDecl> {
    web_type("list")
}
fn web_kanban() -> ViewResult<ViewTypeDecl> {
    web_type("kanban")
}
static WEB_TYPES: [ViewTypeDeclFn; 2] = [web_list, web_kanban];

fn table(orm: &Orm) -> Arc<PublishedViewTypes> {
    orm.env(SUPERUSER_ID, Context::new())
        .generation_attachment::<PublishedViewTypes>()
        .unwrap()
}

/// ORM it builds, each bound to its own registry and closure; the owner binding itself is tested
/// without a database in `view_type_publication.rs`)
#[tokio::test]
async fn root_and_stage_orms_of_the_module_system_publish_the_view_types() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .view_types([ViewTypeContribution::new(&WEB_ADDON, &WEB_TYPES)])
        .build()
        .unwrap();
    // Root ORM: `base` and the implicit descriptors.
    let root = system
        .system_env(Context::new())
        .generation_attachment::<PublishedViewTypes>()
        .unwrap();
    assert!(root.find("list").is_some());
    assert_eq!(root.keys_visible_to("base"), Vec::<&str>::new());
    assert_eq!(root.keys_visible_to("web"), vec!["kanban", "list"]);

    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let initialized = table(&init.orm);
    assert!(!Arc::ptr_eq(&root, &initialized));

    // A stage that installs `note` publishes a table whose closure includes it.
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let staged = table(&installed.orm);
    assert!(!Arc::ptr_eq(&initialized, &staged));
    assert!(staged.find("list").is_some());
    assert!(
        staged.find("kanban").is_some(),
        "nonbuiltin dependency refusal must test an actually published declaration"
    );
    // nonbuiltin keys still require their owner's closure (lead20261103T192149Z-df3a1b).
    assert_eq!(staged.keys_visible_to("note"), Vec::<&str>::new());
    staged.require_key_for("note", "list").unwrap();
    let error = staged.require_key_for("note", "kanban").unwrap_err();
    assert_eq!(error, erp_views::ViewError::InvalidDescription(
        "View-type key \"kanban\" belongs to addon \"web\", which is not in the installed dependency closure of addon \"note\"".to_owned()));
    assert_eq!(
        error.to_string(),
        "View-type key \"kanban\" belongs to addon \"web\", which is not in the installed dependency closure of addon \"note\""
    );
    assert_eq!(staged.keys_visible_to("web"), vec!["kanban", "list"]);
    // The earlier generation has its own table, which does not know `note`.
    let error = initialized.require_key_for("note", "list").unwrap_err();
    assert_eq!(
        error,
        erp_views::ViewError::InvalidDescription(
            "Addon \"note\" is not in the installed set".to_owned()
        )
    );
    assert_eq!(
        error.to_string(),
        "Addon \"note\" is not in the installed set"
    );
    assert!(initialized.keys_visible_to("note").is_empty());
}
