//! Publication of the owner-bound view types: installed closure, method binding, key visibility
//! and selection reconciliation (plan WP3b). No database, no ORM.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder, WEB_ADDON};
use erp_views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    InstalledClosure, MetadataFieldSet, OwnerBoundViewTypes, PublishedViewTypes, SchemaNode,
    SchemaProperty, ViewResult, ViewSchema, ViewTypeContribution, ViewTypeDecl, ViewTypeDeclFn,
};
use std::sync::Arc;

static EXAMPLE: AddonDescriptor = AddonDescriptor {
    name: "example",
    depends: &["web"],
    exact_only: false,
};
static OTHER: AddonDescriptor = AddonDescriptor {
    name: "other",
    depends: &["web"],
    exact_only: false,
};
type Deps = &'static [&'static str];
const NONE: Deps = &[];
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
fn example_timeline() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", &[])
}
fn example_with_method() -> ViewResult<ViewTypeDecl> {
    declare("example", "example.timeline", &["do_thing"])
}
static WEB_TYPES: [ViewTypeDeclFn; 1] = [web_list];
static WEB_TWICE: [ViewTypeDeclFn; 2] = [web_list, web_list];
static EXAMPLE_TYPES: [ViewTypeDeclFn; 1] = [example_timeline];
static EXAMPLE_METHOD_TYPES: [ViewTypeDeclFn; 1] = [example_with_method];

static VIEW_MODEL: ModelDecl = ModelDecl::new("base", "ir.ui.view").fields(
    const {
        &[FieldDecl::selection(
            "type",
            &[("list", "List"), ("form", "Form")],
        )]
    },
);
static OTHER_ADDS: ModelDecl = ModelDecl::extension("other", "ir.ui.view", &["ir.ui.view"]).fields(
    const {
        &[FieldDecl::selection_add(
            "type",
            &[("example.timeline", Some("Timeline"))],
        )]
    },
);
static EXAMPLE_ADDS: ModelDecl = ModelDecl::extension("example", "ir.ui.view", &["ir.ui.view"])
    .fields(
        const {
            &[FieldDecl::selection_add(
                "type",
                &[("example.timeline", Some("Timeline"))],
            )]
        },
    );
static EXAMPLE_ADDS_FOREIGN: ModelDecl =
    ModelDecl::extension("example", "ir.ui.view", &["ir.ui.view"]).fields(
        const {
            &[FieldDecl::selection_add(
                "type",
                &[("gantt", Some("Gantt"))],
            )]
        },
    );
static EXAMPLE_ADDS_UNKNOWN: ModelDecl =
    ModelDecl::extension("example", "ir.ui.view", &["ir.ui.view"]).fields(
        const {
            &[FieldDecl::selection_add(
                "type",
                &[("mystery", Some("Mystery"))],
            )]
        },
    );

fn closure() -> InstalledClosure {
    InstalledClosure::new(&[
        ("base", NONE),
        ("web", &["base"] as Deps),
        ("example", &["web"] as Deps),
        ("other", &["web"] as Deps),
    ])
    .unwrap()
}
fn make_registry(decls: &[&'static ModelDecl]) -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&EXAMPLE, &OTHER])
            .with_decls(decls.iter().copied())
            .build()
            .unwrap(),
    )
}
fn bound(
    registry: &Arc<Registry>,
    web: &'static [ViewTypeDeclFn],
    example: &'static [ViewTypeDeclFn],
) -> ViewResult<OwnerBoundViewTypes> {
    OwnerBoundViewTypes::bind(
        registry.clone(),
        &[
            ViewTypeContribution::new(&WEB_ADDON, web),
            ViewTypeContribution::new(&EXAMPLE, example),
        ],
    )
}
fn publish(
    registry: &Arc<Registry>,
    example: &'static [ViewTypeDeclFn],
) -> ViewResult<PublishedViewTypes> {
    PublishedViewTypes::publish(bound(registry, &WEB_TYPES, example)?, closure(), |_, _| {
        false
    })
}

/// data-install check is the next test)
#[test]
fn publication_binds_declarations_to_owners_and_exposes_closure_visibility() {
    let registry = make_registry(&[&VIEW_MODEL]);
    let published = publish(&registry, &EXAMPLE_TYPES).unwrap();
    published.require_registry(&registry).unwrap();
    assert!(std::ptr::eq(published.owner("list").unwrap(), &WEB_ADDON));
    assert!(std::ptr::eq(
        published.owner("example.timeline").unwrap(),
        &EXAMPLE
    ));
    assert_eq!(
        published.keys_visible_to("example"),
        vec!["example.timeline", "list"]
    );
    assert_eq!(published.keys_visible_to("other"), vec!["list"]);
    assert!(published.keys_visible_to("base").is_empty());
    assert!(published.keys_visible_to("unknown").is_empty());
    assert!(published.find("list").is_some());
    assert!(published.find("kanban").is_none());
}

/// error; the other refusals are WP3a)
#[test]
fn an_unresolved_data_method_binding_refuses_publication() {
    let registry = make_registry(&[&VIEW_MODEL]);
    let error = PublishedViewTypes::publish(
        bound(&registry, &WEB_TYPES, &EXAMPLE_METHOD_TYPES).unwrap(),
        closure(),
        |_, _| false,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "View-type declaration binds the unknown data method \"do_thing\" (view type \"example.timeline\" of addon \"example\")"
    );
    // The resolver receives the owner's dependency closure and the method name.
    let mut seen = Vec::new();
    PublishedViewTypes::publish(
        bound(&registry, &WEB_TYPES, &EXAMPLE_METHOD_TYPES).unwrap(),
        closure(),
        |scope, method| {
            seen.push((scope.iter().cloned().collect::<Vec<_>>(), method.to_owned()));
            true
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        vec![(
            vec!["base".to_owned(), "example".to_owned(), "web".to_owned()],
            "do_thing".to_owned()
        )]
    );
}

#[test]
fn a_duplicate_key_refuses_publication() {
    let registry = make_registry(&[&VIEW_MODEL]);
    let error = bound(&registry, &WEB_TWICE, &EXAMPLE_TYPES).unwrap_err();
    assert_eq!(
        error.to_string(),
        "View-type declaration registers a duplicate key (view type \"list\" of addon \"web\")"
    );
}

#[test]
fn a_closure_with_an_absent_dependency_refuses() {
    let error =
        InstalledClosure::new(&[("base", NONE), ("example", &["web"] as Deps)]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Installed addon \"example\" depends on \"web\", which is not in the installed set"
    );
    let error = InstalledClosure::new(&[("base", NONE), ("base", NONE)]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Installed addon \"base\" is listed twice"
    );
}

#[test]
fn a_key_outside_the_declaring_addons_dependency_closure_is_refused() {
    let registry = make_registry(&[&VIEW_MODEL]);
    let published = publish(&registry, &EXAMPLE_TYPES).unwrap();
    published.require_key_for("example", "list").unwrap();
    published
        .require_key_for("example", "example.timeline")
        .unwrap();
    // `qweb` is the base template kind, not a data view mode.
    published.require_key_for("other", "qweb").unwrap();
    assert_eq!(
        published
            .require_key_for("other", "example.timeline")
            .unwrap_err()
            .to_string(),
        "View-type key \"example.timeline\" belongs to addon \"example\", which is not in the installed dependency closure of addon \"other\""
    );
    // A reserved key whose owner is not installed is refused too, published or not.
    assert_eq!(
        published
            .require_key_for("example", "gantt")
            .unwrap_err()
            .to_string(),
        "View-type key \"gantt\" belongs to addon \"web_gantt\", which is not in the installed dependency closure of addon \"example\""
    );
    // Owner in the closure but nothing registered: the selection value alone does not serve.
    assert_eq!(
        published
            .require_key_for("example", "kanban")
            .unwrap_err()
            .to_string(),
        "View-type key \"kanban\" is not registered in the published view types"
    );
    assert_eq!(
        published
            .require_key_for("example", "mystery")
            .unwrap_err()
            .to_string(),
        "View-type key \"mystery\" is neither a reserved bare key nor in an addon namespace"
    );
    assert_eq!(
        published
            .require_key_for("nobody", "list")
            .unwrap_err()
            .to_string(),
        "Addon \"nobody\" is not in the installed set"
    );
}

#[test]
fn only_the_owner_contributes_a_view_type_selection_value() {
    // The owner adds its own key: accepted.
    let registry = make_registry(&[&VIEW_MODEL, &EXAMPLE_ADDS]);
    publish(&registry, &EXAMPLE_TYPES).unwrap();
    // Another addon adds the owner's key: refused.
    let registry = make_registry(&[&VIEW_MODEL, &OTHER_ADDS]);
    assert_eq!(
        publish(&registry, &EXAMPLE_TYPES).unwrap_err().to_string(),
        "Addon \"other\" adds view-type key \"example.timeline\" to ir.ui.view.type, but only its owner \"example\" may contribute it"
    );
    // A reserved key added by an addon that is not its owner: refused.
    let registry = make_registry(&[&VIEW_MODEL, &EXAMPLE_ADDS_FOREIGN]);
    assert_eq!(
        publish(&registry, &EXAMPLE_TYPES).unwrap_err().to_string(),
        "Addon \"example\" adds view-type key \"gantt\" to ir.ui.view.type, but only its owner \"web_gantt\" may contribute it"
    );
    // An unknown bare key is refused with the compatibility-table diagnostic.
    let registry = make_registry(&[&VIEW_MODEL, &EXAMPLE_ADDS_UNKNOWN]);
    assert_eq!(
        publish(&registry, &EXAMPLE_TYPES).unwrap_err().to_string(),
        "View-type key \"mystery\" is neither a reserved bare key nor in an addon namespace"
    );
}

/// with equal contents)
#[test]
fn a_published_table_is_bound_to_its_exact_registry() {
    let first = make_registry(&[&VIEW_MODEL]);
    let published = publish(&first, &EXAMPLE_TYPES).unwrap();
    let equal = make_registry(&[&VIEW_MODEL]);
    assert_eq!(
        published.require_registry(&equal).unwrap_err().to_string(),
        "View-type registry identity changed; compose a new generation before serving"
    );
}

/// publication when its owner has not contributed the corresponding H18 declaration)
#[test]
fn selection_addition_without_a_registration_refuses_publication() {
    let registry = make_registry(&[&VIEW_MODEL, &EXAMPLE_ADDS]);
    let error = publish(&registry, &[]).unwrap_err();
    assert!(matches!(error, erp_views::ViewError::InvalidDescription(_)));
    assert_eq!(
        error.to_string(),
        "Addon \"example\" adds view-type key \"example.timeline\" to ir.ui.view.type without publishing its H18 declaration"
    );
}

/// other data-view key still requires its owner's dependency, lead A20261103T192149Z-df3a1b)
#[test]
fn framework_builtins_need_no_web_dependency() {
    let registry = make_registry(&[&VIEW_MODEL]);
    let published = publish(&registry, &EXAMPLE_TYPES).unwrap();
    for key in ["list", "form", "search"] {
        published.require_key_for("base", key).unwrap();
    }
    let error = published.require_key_for("base", "kanban").unwrap_err();
    assert!(matches!(error, erp_views::ViewError::InvalidDescription(_)));
    assert_eq!(
        error.to_string(),
        "View-type key \"kanban\" belongs to addon \"web\", which is not in the installed dependency closure of addon \"base\""
    );
}
