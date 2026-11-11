//! The mandatory H18 declaration contract (WP3a): a validated
//! builder is the only way to make a `ViewTypeDecl`, and `finish` refuses an incomplete or
//! malformed declaration with a located typed error.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_expr::{parse, wire::ValidatedAst};
use erp_views::wire::ViewNode;
use erp_views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder, FieldPredicate,
    FieldReference, MetadataFieldSet, ReferenceVisitor, SchemaNode, SchemaProperty, ViewError,
    ViewResult, ViewSchema, ViewTypeDecl, ViewTypeDeclBuilder, find_view_type, form_schema,
    list_schema, search_schema, validate_view_types,
};
use serde_json::{Value, json};

static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static ROOT: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("label", &TEXT)]);

fn test_schema() -> ViewSchema {
    ViewSchema::new(1, &ROOT).unwrap()
}
fn ok_validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
    budget.check()
}
fn ok_build(
    _: &CheckedViewInput<'_>,
    root: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    root.object(|object| object.set("label", |value| value.text("x")))
}
fn arch_fields() -> MetadataFieldSet {
    MetadataFieldSet::builder()
        .arch_references()
        .finish()
        .unwrap()
}
fn complete(addon: &'static str, key: &'static str) -> ViewTypeDeclBuilder {
    ViewTypeDecl::builder(addon, key)
        .wire_major(1)
        .renderer("list")
        .schema(&test_schema())
        .validator(ok_validate)
        .builder(ok_build)
        .metadata_fields(arch_fields())
        .methods(&[])
}
fn located(reason: &str, addon: &str, key: &str) -> ViewError {
    ViewError::InvalidDescription(format!(
        "View-type declaration {reason} (view type {key:?} of addon {addon:?})"
    ))
}

/// the registry admission; contributing the `ir.ui.view.type` and `view_mode` selection values is
/// publication, WP3b)
#[test]
fn owner_registrations_for_reserved_bare_keys_are_accepted() {
    let web: Vec<ViewTypeDecl> = ["list", "form", "search"]
        .into_iter()
        .map(|key| complete("web", key).finish().unwrap())
        .collect();
    let others = [
        complete("mail", "activity").finish().unwrap(),
        complete("web_gantt", "gantt").finish().unwrap(),
        complete("web_gantt", "web_gantt.gantt").finish().unwrap(),
    ];
    let all: Vec<&ViewTypeDecl> = web.iter().chain(others.iter()).collect();
    validate_view_types(&all).unwrap();
    let found = find_view_type(&all, "form").unwrap();
    assert_eq!(
        (found.addon(), found.key(), found.wire_major()),
        ("web", "form", 1)
    );
    assert!(find_view_type(&all, "kanban").is_none());
}

/// an installed registry; a missing owner and a data method that is not an exposed model method
/// need the installed generation, WP3b)
#[test]
fn incomplete_declarations_are_refused_at_finish_with_a_located_error() {
    let cases: Vec<(&str, ViewTypeDeclBuilder)> = vec![
        (
            "is missing its schema",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .renderer("list")
                .validator(ok_validate)
                .builder(ok_build)
                .metadata_fields(arch_fields())
                .methods(&[]),
        ),
        (
            "is missing its validator",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .renderer("list")
                .schema(&test_schema())
                .builder(ok_build)
                .metadata_fields(arch_fields())
                .methods(&[]),
        ),
        (
            "is missing its builder",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .renderer("list")
                .schema(&test_schema())
                .validator(ok_validate)
                .metadata_fields(arch_fields())
                .methods(&[]),
        ),
        (
            "is missing its metadata field set",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .renderer("list")
                .schema(&test_schema())
                .validator(ok_validate)
                .builder(ok_build)
                .methods(&[]),
        ),
        (
            "is missing its renderer",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .schema(&test_schema())
                .validator(ok_validate)
                .builder(ok_build)
                .metadata_fields(arch_fields())
                .methods(&[]),
        ),
        (
            "is missing its data-method list",
            ViewTypeDecl::builder("web", "list")
                .wire_major(1)
                .renderer("list")
                .schema(&test_schema())
                .validator(ok_validate)
                .builder(ok_build)
                .metadata_fields(arch_fields()),
        ),
        (
            "is missing its wire major",
            ViewTypeDecl::builder("web", "list")
                .renderer("list")
                .schema(&test_schema())
                .validator(ok_validate)
                .builder(ok_build)
                .metadata_fields(arch_fields())
                .methods(&[]),
        ),
    ];
    for (reason, builder) in cases {
        assert_eq!(
            builder.finish().unwrap_err(),
            located(reason, "web", "list")
        );
    }
}

#[test]
fn unknown_or_repeated_data_method_names_are_refused() {
    static EMPTY_NAME: [&str; 1] = [""];
    static UPPER: [&str; 1] = ["Action_Confirm"];
    static PRIVATE: [&str; 1] = ["_compute_total"];
    static SPACED: [&str; 1] = ["read group"];
    static TWICE: [&str; 2] = ["web_read", "web_read"];
    let invalid = located("declares an invalid data method name", "web", "list");
    for names in [&EMPTY_NAME[..], &UPPER, &PRIVATE, &SPACED] {
        assert_eq!(
            complete("web", "list").methods(names).finish().unwrap_err(),
            invalid
        );
    }
    assert_eq!(
        complete("web", "list")
            .methods(&TWICE)
            .finish()
            .unwrap_err(),
        located("declares a data method twice", "web", "list")
    );
    let many: &'static [&'static str] = Box::leak(
        (0..65)
            .map(|i| &*Box::leak(format!("method_{i}").into_boxed_str()))
            .collect::<Vec<&'static str>>()
            .into_boxed_slice(),
    );
    assert_eq!(
        complete("web", "list").methods(many).finish().unwrap_err(),
        located("declares too many data methods", "web", "list")
    );
    static GOOD: [&str; 2] = ["web_read", "action_confirm"];
    let decl = complete("web", "list").methods(&GOOD).finish().unwrap();
    assert_eq!(decl.methods(), &GOOD);
}

/// reload-once shell behaviour and VersionMismatch are serving, WP4/WP7)
#[test]
fn wire_major_zero_is_refused_and_one_is_accepted() {
    assert_eq!(
        complete("web", "list").wire_major(0).finish().unwrap_err(),
        located("has unsupported wire major 0", "web", "list")
    );
    assert_eq!(
        complete("web", "list")
            .wire_major(1)
            .finish()
            .unwrap()
            .wire_major(),
        1
    );
}

/// in `duplicate_keys_are_refused_even_from_the_same_owner`)
#[test]
fn key_owner_and_namespace_rules_apply_at_finish() {
    let cases = [
        ("Web", "list", "has an invalid addon identifier"),
        ("web", "list-invalid", "has an invalid key"),
        ("web", "a.b.c", "has an invalid key"),
        (
            "my_addon",
            "other_addon.view",
            "has a namespaced key whose prefix is not its owning addon",
        ),
        (
            "custom_addon",
            "my_view",
            "has an unreserved bare key; custom view types must use <addon>.<name>",
        ),
        (
            "wrong_addon",
            "list",
            "has a bare key reserved for another owning addon",
        ),
    ];
    for (addon, key, reason) in cases {
        assert_eq!(
            complete(addon, key).finish().unwrap_err(),
            located(reason, addon, key)
        );
    }
    complete("my_addon", "my_addon.view").finish().unwrap();
}

/// declaration; the Unsupported/Validation answers and the generated-view identity are serving,
/// WP4/WP7)
#[test]
fn qweb_and_an_empty_renderer_cannot_be_declared() {
    assert_eq!(
        complete("web", "qweb").finish().unwrap_err(),
        located(
            "has an unreserved bare key; custom view types must use <addon>.<name>",
            "web",
            "qweb"
        )
    );
    assert_eq!(
        complete("web", "list").renderer("").finish().unwrap_err(),
        located("has an invalid renderer", "web", "list")
    );
}

#[test]
fn duplicate_keys_are_refused_even_from_the_same_owner() {
    let first = complete("web", "list").finish().unwrap();
    let second = complete("web", "list").renderer("other").finish().unwrap();
    let other = complete("web", "form").finish().unwrap();
    assert_eq!(
        validate_view_types(&[&first, &other, &second]).unwrap_err(),
        located("registers a duplicate key", "web", "list")
    );
    validate_view_types(&[&first, &other]).unwrap();
}

#[test]
fn a_finished_declaration_exposes_a_visitor_derived_from_its_own_schema() {
    let decl = complete("web", "list").finish().unwrap();
    assert_eq!(decl.schema().revision(), 1);
    assert_eq!(decl.renderer(), "list");
    let budget = DescriptionBudget::new();
    let payload = json!({"label": "x"});
    let refs = decl.visitor().references(&payload, &budget).unwrap();
    assert_eq!(refs.fields().count(), 0);
    assert_eq!(
        decl.visitor()
            .references(&json!({"label": 1}), &budget)
            .unwrap_err(),
        ViewError::InvalidDescription("Description property requires text".to_owned())
    );
    assert!(decl.metadata_fields().includes_arch_references());
}

// ---- MetadataFieldSet --------------------------------------------------------------------------

fn field_error(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}

/// (metadata-field-set declaration; VD-06): union of explicit handles, arch references
/// and registry predicates, validated before use.
#[test]
fn metadata_field_set_is_the_union_of_handles_arch_references_and_predicates() {
    let set = MetadataFieldSet::builder()
        .root_path("name")
        .model_path("res.partner", "company_id.name")
        .arch_references()
        .predicate(FieldPredicate::AllReadable)
        .predicate(FieldPredicate::Groupable)
        .finish()
        .unwrap();
    assert!(set.includes_arch_references());
    assert_eq!(
        set.predicates(),
        &[FieldPredicate::AllReadable, FieldPredicate::Groupable]
    );
    assert_eq!(
        set.explicit().collect::<Vec<_>>(),
        vec![
            FieldReference::RootPath("name"),
            FieldReference::ModelPath {
                model: "res.partner",
                path: "company_id.name"
            }
        ]
    );
    assert!(
        !MetadataFieldSet::builder()
            .root_path("name")
            .finish()
            .unwrap()
            .includes_arch_references()
    );
}

#[test]
fn metadata_field_set_refuses_empty_invalid_repeated_and_unknown_members() {
    let refuse = |builder: erp_views::MetadataFieldSetBuilder, message: &str| {
        assert_eq!(builder.finish().unwrap_err(), field_error(message));
    };
    refuse(
        MetadataFieldSet::builder(),
        "Metadata field set declares nothing; name a handle, arch references or a predicate",
    );
    refuse(
        MetadataFieldSet::builder().root_path("bad path"),
        "Metadata field set has an invalid field path",
    );
    refuse(
        MetadataFieldSet::builder().root_path(""),
        "Metadata field set has an invalid field path",
    );
    refuse(
        MetadataFieldSet::builder().model_path("", "name"),
        "Metadata field set has an invalid model",
    );
    refuse(
        MetadataFieldSet::builder()
            .root_path("name")
            .root_path("name"),
        "Metadata field set repeats a field handle",
    );
    refuse(
        MetadataFieldSet::builder()
            .predicate(FieldPredicate::Numeric)
            .predicate(FieldPredicate::Numeric),
        "Metadata field set repeats a predicate",
    );
    let mut many = MetadataFieldSet::builder();
    for i in 0..257 {
        many = many.root_path(&format!("field_{i}"));
    }
    refuse(many, "Metadata field set exceeds 256 entries");
    assert_eq!(
        FieldPredicate::from_name("numeric").unwrap(),
        FieldPredicate::Numeric
    );
    assert_eq!(FieldPredicate::Groupable.name(), "groupable");
    assert_eq!(
        FieldPredicate::from_name("writable").unwrap_err(),
        field_error("Unknown metadata field predicate")
    );
}

// ---- builtin closed schemas --------------------------------------------------------------------

fn admitted_document(source: &str) -> Value {
    let ast = ValidatedAst::from_expr(&parse(source).unwrap()).unwrap();
    serde_json::from_slice(&ast.to_json().unwrap()).unwrap()
}
fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(name, value)| (name, strip_nulls(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}
/// A real wire node: it must deserialize as `wire::ViewNode`, so the sample is not invented.
fn wire_node(sample: Value) -> Value {
    let node: ViewNode = serde_json::from_value(sample).expect("sample is a valid wire node");
    // Absent and null are the same thing in a description payload (no nullable slot exists).
    strip_nulls(serde_json::to_value(node).unwrap())
}
fn arena(nodes: Vec<Value>) -> Value {
    json!({"root": 0, "nodes": nodes.into_iter().map(wire_node).collect::<Vec<_>>()})
}
fn modifiers() -> Value {
    json!({"readonly": null, "required": null, "invisible": admitted_document("state == 'draft'")})
}
fn no_modifiers() -> Value {
    json!({"readonly": null, "required": null, "invisible": null})
}
fn field_node() -> Value {
    json!({"kind": "field", "field": {"model": "res.partner", "name": "name"}, "widget": "char",
        "options": {}, "modifiers": modifiers(), "label": "Name", "force_save": false})
}
fn references_of(schema: ViewSchema, payload: &Value) -> ViewResult<Vec<String>> {
    let budget = DescriptionBudget::new();
    let visitor = ReferenceVisitor::new(schema);
    let refs = visitor.references(payload, &budget)?;
    let mut out: Vec<String> = refs
        .fields()
        .map(|field| match field {
            FieldReference::RootPath(path) => format!("root:{path}"),
            FieldReference::ModelPath { model, path } => format!("{model}:{path}"),
            _ => panic!("unexpected reference"),
        })
        .collect();
    out.sort();
    Ok(out)
}

/// registration that names them is not refused for its schema)
#[test]
fn builtin_schemas_validate_under_the_registration_caps() {
    for schema in [list_schema(), form_schema(), search_schema()] {
        assert_eq!(schema.unwrap().revision(), 1);
    }
}

#[test]
fn list_schema_accepts_the_wire_list_and_field_nodes_and_derives_their_references() {
    let list = json!({"kind": "list", "children": [1], "page_limit": 80,
        "search_panel": {"kind": "selection", "field": {"model": "res.partner", "name": "state"},
            "label": "State", "enable_counters": false, "expand": true,
            "default_value": null, "notice": null}});
    let payload = arena(vec![list, field_node()]);
    assert_eq!(
        references_of(list_schema().unwrap(), &payload).unwrap(),
        vec!["res.partner:name", "res.partner:state", "root:state"]
    );
    let unsupported = arena(vec![
        json!({"kind": "list", "children": [], "search_panel": {"kind": "unsupported", "message": "no"}}),
    ]);
    references_of(list_schema().unwrap(), &unsupported).unwrap();
}

#[test]
fn list_schema_refuses_foreign_kinds_undeclared_members_and_missing_members() {
    let schema = || list_schema().unwrap();
    let foreign = json!({"root": 0, "nodes": [{"kind": "sheet", "children": []}]});
    assert_eq!(
        references_of(schema(), &foreign).unwrap_err(),
        field_error("Description tagged union has an undeclared variant")
    );
    let extra = json!({"root": 0, "nodes": [{"kind": "list", "children": [], "colour": "red"}]});
    assert_eq!(
        references_of(schema(), &extra).unwrap_err(),
        field_error("Description payload has an undeclared property")
    );
    let missing = json!({"root": 0, "nodes": [{"kind": "list"}]});
    assert_eq!(
        references_of(schema(), &missing).unwrap_err(),
        field_error("Description payload is missing a required property")
    );
    let no_root = json!({"nodes": []});
    assert_eq!(
        references_of(schema(), &no_root).unwrap_err(),
        field_error("Description payload is missing a required property")
    );
}

#[test]
fn form_schema_accepts_every_wire_form_node_kind() {
    let button = |kind: &str| {
        json!({"kind": kind, "name": "action_post", "button_type": "object", "label": "Post",
            "variant": "primary", "confirm": null, "modifiers": no_modifiers(), "children": []})
    };
    let mut stat = button("stat_button");
    let mut plain = button("button");
    plain.as_object_mut().unwrap().remove("children");
    stat["confirm"] = json!("Sure?");
    let nodes = vec![
        json!({"kind": "form", "record_mode": "record", "children": [1]}),
        json!({"kind": "sheet", "children": []}),
        json!({"kind": "title", "children": []}),
        json!({"kind": "heading", "level": 1, "children": []}),
        json!({"kind": "aside", "children": []}),
        json!({"kind": "header", "children": []}),
        json!({"kind": "button_box", "children": []}),
        stat,
        json!({"kind": "footer", "children": []}),
        json!({"kind": "cancel_button", "label": "Cancel", "variant": "secondary",
            "modifiers": no_modifiers()}),
        plain,
        json!({"kind": "group", "label": "", "columns": 2, "colspan": 1, "children": []}),
        json!({"kind": "notebook", "children": []}),
        json!({"kind": "page", "name": "main", "label": "Main", "children": []}),
        json!({"kind": "separator", "label": "Sep"}),
        field_node(),
        json!({"kind": "field", "field": {"model": "crm.lead", "name": "stage_id"},
            "widget": "statusbar", "options": {"visible": ["new", "won"]},
            "modifiers": no_modifiers(), "label": "Stage", "force_save": true}),
        json!({"kind": "widget", "widget": "web.settings",
            "options": {"scope_kinds": ["global", "company"], "app": null}, "modifiers": no_modifiers()}),
        json!({"kind": "widget", "widget": "web.settings",
            "options": {"scope_kinds": ["global"], "app": "acceptance_notes"}, "modifiers": no_modifiers()}),
        json!({"kind": "widget", "widget": "web.addons", "options": {}, "modifiers": no_modifiers()}),
    ];
    assert_eq!(
        references_of(form_schema().unwrap(), &arena(nodes)).unwrap(),
        vec!["crm.lead:stage_id", "res.partner:name", "root:state"]
    );
}

#[test]
fn search_schema_accepts_every_wire_search_node_kind_with_scoped_expression_references() {
    let nodes = vec![
        json!({"kind": "search", "children": [1]}),
        json!({"kind": "search_field", "field": {"model": "res.partner", "name": "name"},
            "label": "Name", "operator": "ilike",
            "filter_domain": admitted_document("[('email', 'ilike', 'x')]")}),
        json!({"kind": "search_filter", "name": "mine", "label": "Mine",
            "domain": admitted_document("[('user_id', '=', 1)]"),
            "context": admitted_document("{'group_by': 'stage_id:month'}")}),
        json!({"kind": "search_separator"}),
        json!({"kind": "search_group", "label": "Group by", "expand": true, "children": []}),
    ];
    assert_eq!(
        references_of(search_schema().unwrap(), &arena(nodes)).unwrap(),
        vec![
            "res.partner:name",
            "root:email",
            "root:stage_id",
            "root:user_id"
        ]
    );
    let hidden = arena(vec![
        json!({"kind": "search_filter", "name": "x", "label": "X",
        "domain": admitted_document("[('partner_id.country_id', '=', 1)]")}),
    ]);
    assert_eq!(
        references_of(search_schema().unwrap(), &hidden).unwrap(),
        vec!["root:partner_id.country_id"]
    );
}
