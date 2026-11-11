use crate::registry::{find_view_type, is_valid_view_type_key, validate_view_types};
use crate::wire::{FieldRef, ViewNode};
use crate::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
    MetadataFieldSet, SchemaNode, SchemaProperty, ViewResult, ViewSchema, ViewTypeDecl,
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
/// A complete declaration; `finish` is the only constructor, so every field is validated.
fn decl(
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

#[test]
fn test_view_type_registry_lookup() {
    let list = decl("web", "list", "list").unwrap();
    let form = decl("web", "form", "form").unwrap();
    let search = decl("web", "search", "search").unwrap();
    let registry = [&list, &form, &search];
    assert_eq!(
        find_view_type(&registry, "list").map(|d| d.key()),
        Some("list")
    );
    assert_eq!(
        find_view_type(&registry, "form").map(|d| d.renderer()),
        Some("form")
    );
    assert_eq!(
        find_view_type(&registry, "search").map(|d| d.addon()),
        Some("web")
    );
    assert!(find_view_type(&registry, "unknown").is_none());
}

#[test]
fn test_view_type_registry_validation() {
    let valid = [
        decl("web", "list", "list").unwrap(),
        decl("web", "form", "form").unwrap(),
        decl("web", "search", "search").unwrap(),
        decl("web_gantt", "web_gantt.gantt", "gantt").unwrap(),
        decl("web_gantt", "gantt", "gantt").unwrap(),
        decl("mail", "activity", "activity").unwrap(),
    ];
    let refs: Vec<&ViewTypeDecl> = valid.iter().collect();
    assert!(validate_view_types(&refs).is_ok());

    let dup = [
        decl("web", "list", "list").unwrap(),
        decl("web", "list", "other_list").unwrap(),
    ];
    assert!(validate_view_types(&[&dup[0], &dup[1]]).is_err());

    // Every other per-declaration rule is refused by `finish` (tests/view_type_decl.rs).
    assert!(decl("Web", "list", "list").is_err());
    assert!(decl("web", "list-invalid", "list").is_err());
    assert!(decl("custom_addon", "my_view", "my_view").is_err());
    assert!(decl("wrong_addon", "list", "list").is_err());
    assert!(decl("my_addon", "other_addon.view", "view").is_err());
    assert!(decl("my_addon", "my_addon.view", "view").is_ok());

    assert!(is_valid_view_type_key("list"));
    assert!(is_valid_view_type_key("custom_view"));
    assert!(is_valid_view_type_key("web.search"));
    assert!(!is_valid_view_type_key(""));
    assert!(!is_valid_view_type_key("123bad"));
    assert!(!is_valid_view_type_key("a.b.c"));
}

#[test]
fn test_search_view_nodes_wire_serde() {
    let search_root = ViewNode::Search {
        children: vec![1, 2, 3, 4],
        search_panel: None,
    };
    let json = serde_json::to_string(&search_root).expect("serialize Search root");
    let back: ViewNode = serde_json::from_str(&json).expect("deserialize Search root");
    match back {
        ViewNode::Search { children, .. } => assert_eq!(children, vec![1, 2, 3, 4]),
        other => panic!("expected ViewNode::Search, got {other:?}"),
    }

    let search_field = ViewNode::SearchField {
        field: FieldRef::new("res.partner".to_string(), "name".to_string()),
        label: "Name".to_string(),
        filter_domain: None,
        operator: Some("ilike".to_string()),
    };
    let json = serde_json::to_string(&search_field).expect("serialize SearchField");
    let back: ViewNode = serde_json::from_str(&json).expect("deserialize SearchField");
    match back {
        ViewNode::SearchField {
            field,
            label,
            filter_domain,
            operator,
        } => {
            assert_eq!(field.model, "res.partner");
            assert_eq!(field.name, "name");
            assert_eq!(label, "Name");
            assert!(filter_domain.is_none());
            assert_eq!(operator, Some("ilike".to_string()));
        }
        other => panic!("expected ViewNode::SearchField, got {other:?}"),
    }

    let search_filter = ViewNode::SearchFilter {
        name: "active_only".to_string(),
        label: "Active".to_string(),
        domain: None,
        context: None,
    };
    let json = serde_json::to_string(&search_filter).expect("serialize SearchFilter");
    let back: ViewNode = serde_json::from_str(&json).expect("deserialize SearchFilter");
    match back {
        ViewNode::SearchFilter { name, label, .. } => {
            assert_eq!(name, "active_only");
            assert_eq!(label, "Active");
        }
        other => panic!("expected ViewNode::SearchFilter, got {other:?}"),
    }

    let separator = ViewNode::SearchSeparator {};
    let json = serde_json::to_string(&separator).expect("serialize SearchSeparator");
    let back: ViewNode = serde_json::from_str(&json).expect("deserialize SearchSeparator");
    assert!(matches!(back, ViewNode::SearchSeparator {}));

    let group = ViewNode::SearchGroup {
        label: "Group By".to_string(),
        expand: true,
        children: vec![5],
    };
    let json = serde_json::to_string(&group).expect("serialize SearchGroup");
    let back: ViewNode = serde_json::from_str(&json).expect("deserialize SearchGroup");
    match back {
        ViewNode::SearchGroup {
            label,
            expand,
            children,
        } => {
            assert_eq!(label, "Group By");
            assert!(expand);
            assert_eq!(children, vec![5]);
        }
        other => panic!("expected ViewNode::SearchGroup, got {other:?}"),
    }
}

#[test]
fn standalone_search_panel_serde_is_typed_and_closed() {
    let node: ViewNode = serde_json::from_str(r#"{"kind":"search","children":[],"search_panel":{"kind":"unsupported","message":"a later release"}}"#).unwrap();
    let value = serde_json::to_value(node).unwrap();
    assert_eq!(value["search_panel"]["message"], "a later release");
    assert!(
        serde_json::from_str::<ViewNode>(r#"{"kind":"search","children":[],"unexpected":true}"#)
            .is_err()
    );
}
