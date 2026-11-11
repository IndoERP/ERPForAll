//! Strict section DTO admission; host access and renderer behavior have separate acceptance tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_views::wire::ListSearchPanel;
use serde_json::Value;

fn section_example() -> Value {
    let examples: Value = serde_json::from_str(include_str!(
        "../../../testdata/corpus/search-panel-v1.examples.json"
    ))
    .unwrap();
    examples["positive"][0]["panel"].clone()
}

#[test]
fn typed_sections_preserve_ordered_wire_members() {
    let source = section_example();
    let panel: ListSearchPanel = serde_json::from_value(source.clone()).unwrap();
    assert_eq!(serde_json::to_value(panel).unwrap(), source);
}

#[test]
fn unknown_section_version_is_a_data_error() {
    let mut source = section_example();
    source["version"] = serde_json::json!(2);
    let error = serde_json::from_value::<ListSearchPanel>(source).unwrap_err();
    assert_eq!(error.classify(), serde_json::error::Category::Data);
    assert_eq!(error.to_string(), "unsupported search panel version");
}

#[test]
fn nullable_section_members_cannot_be_omitted() {
    for (section, member) in [(0, "icon"), (1, "icon"), (1, "domain"), (1, "group_by")] {
        let mut source = section_example();
        source["sections"][section]
            .as_object_mut()
            .unwrap()
            .remove(member);
        let error = serde_json::from_value::<ListSearchPanel>(source).unwrap_err();
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert_eq!(error.to_string(), format!("missing field `{member}`"));
    }
}

#[test]
fn section_notice_cannot_be_omitted() {
    let mut source = section_example();
    source.as_object_mut().unwrap().remove("notice");
    let error = serde_json::from_value::<ListSearchPanel>(source).unwrap_err();
    assert_eq!(error.classify(), serde_json::error::Category::Data);
    assert_eq!(error.to_string(), "missing field `notice`");
}

#[test]
fn shared_negative_domain_grammar_is_rejected() {
    let examples: Value = serde_json::from_str(include_str!(
        "../../../testdata/corpus/search-panel-v1.examples.json"
    ))
    .unwrap();
    // The three structural-domain fixtures contain JSON literals also valid in Python after
    // canonicalizing booleans; arbitrary source strings belong to host variable admission.
    for case in examples["negative"].as_array().unwrap() {
        let Some(source) = case.get("domain_source").filter(|source| source.is_array()) else {
            continue;
        };
        let source = serde_json::to_string(source)
            .unwrap()
            .replace("true", "True");
        let parsed = erp_expr::parse(&source).unwrap();
        let ast = erp_expr::wire::ValidatedAst::from_expr(&parsed).unwrap();
        let error = erp_views::validate_panel_domain(&ast).unwrap_err();
        assert_eq!(error, erp_views::ViewError::InvalidDescription(
            "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release".to_owned()
        ));
        assert_eq!(
            error.to_string(),
            "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release"
        );
    }
}

#[derive(Clone, Copy)]
struct CallbackPanel<'a> {
    label: &'a str,
    icon: Option<&'a str>,
    view_key: Option<&'a str>,
    sections: usize,
    views: usize,
}
impl<'a> CallbackPanel<'a> {
    fn new(label: &'a str, sections: usize, views: usize) -> Self {
        Self {
            label,
            icon: None,
            view_key: None,
            sections,
            views,
        }
    }
}

fn callback_filter(
    section: &mut erp_views::DescriptionObjectBuilder<'_>,
    input: CallbackPanel<'_>,
    index: usize,
) -> erp_views::ViewResult<()> {
    section.set("id", |value| {
        value.integer(i64::try_from(index + 1).unwrap())
    })?;
    section.set("field", |value| value.field_handle("panel.item", "tag_ids"))?;
    section.set("label", |value| value.text(input.label))?;
    if let Some(icon) = input.icon {
        section.set("icon", |value| value.text(icon))?;
    }
    section.set("enable_counters", |value| value.boolean(false))?;
    section.set("expand", |value| value.boolean(false))?;
    section.set("limit", |value| value.integer(200))
}

fn callback_sections(
    panel: &mut erp_views::DescriptionObjectBuilder<'_>,
    input: CallbackPanel<'_>,
) -> erp_views::ViewResult<()> {
    panel.set("version", |value| value.integer(1))?;
    panel.set("view_types", |value| {
        value.array(input.views, |index, value| {
            let key = if index == 0 {
                input.view_key.unwrap_or("list").to_owned()
            } else {
                format!("v{index}")
            };
            value.text(&key)
        })
    })?;
    panel.set("sections", |value| {
        value.array(input.sections, |index, value| {
            value.variant("filter", |value| {
                value.object(|section| callback_filter(section, input, index))
            })
        })
    })
}

fn callback_search(
    node: &mut erp_views::DescriptionObjectBuilder<'_>,
    input: CallbackPanel<'_>,
) -> erp_views::ViewResult<()> {
    node.set("children", |value| {
        value.array(0, |_, _| panic!("empty children"))
    })?;
    node.set("search_panel", |value| {
        value.variant("sections", |value| {
            value.object(|panel| callback_sections(panel, input))
        })
    })
}

fn callback_panel_input(
    input: CallbackPanel<'_>,
) -> erp_views::ViewResult<erp_views::AdmittedBuiltinView> {
    use erp_views::{DescriptionBudget, DescriptionBuilder, wire};
    let budget = DescriptionBudget::new();
    let schema = erp_views::search_schema()?;
    let payload = DescriptionBuilder::new(&schema, &budget).build(|value| {
        value.object(|root| {
            root.set("root", |value| value.integer(0))?;
            root.set("nodes", |value| {
                value.array(1, |_, value| {
                    value.variant("search", |value| {
                        value.object(|node| callback_search(node, input))
                    })
                })
            })
        })
    })?;
    payload.builtin_view(
        wire::ViewIdentity::Generated {},
        wire::ViewType::Search,
        "web.search_sections_v1",
        &budget,
    )
}
fn callback_panel(
    label: &str,
    sections: usize,
    views: usize,
) -> erp_views::ViewResult<erp_views::AdmittedBuiltinView> {
    callback_panel_input(CallbackPanel::new(label, sections, views))
}

fn callback_error(label: &str, sections: usize, views: usize, message: &str) {
    let error = callback_panel(label, sections, views).unwrap_err();
    assert_eq!(
        error,
        erp_views::ViewError::InvalidDescription(message.to_owned())
    );
    assert_eq!(error.to_string(), message);
    assert!(!error.to_string().contains(label));
}

fn admitted_panel(label: &str, sections: usize, views: usize) -> Value {
    let admitted = callback_panel(label, sections, views).unwrap();
    serde_json::to_value(admitted.view()).unwrap()["nodes"][0]["search_panel"].clone()
}

#[test]
fn custom_callback_label_byte_limit_is_located() {
    let exact = "é".repeat(128);
    assert_eq!(admitted_panel(&exact, 1, 1)["sections"][0]["label"], exact);
    callback_error(
        &(exact + "x"),
        1,
        1,
        "Search panel section 1 exceeds label byte limit.",
    );
}

#[test]
fn custom_callback_section_count_limit_is_located() {
    assert_eq!(
        admitted_panel("Label", 64, 1)["sections"]
            .as_array()
            .unwrap()
            .len(),
        64
    );
    callback_error(
        "Label",
        65,
        1,
        "Search panel section 65 exceeds section count limit.",
    );
}

#[test]
fn custom_callback_view_count_limit_is_located() {
    assert_eq!(
        admitted_panel("Label", 1, 64)["view_types"]
            .as_array()
            .unwrap()
            .len(),
        64
    );
    callback_error(
        "Label",
        1,
        65,
        "Search panel section 0 exceeds view key count limit.",
    );
}

#[test]
fn custom_callback_icon_byte_limit_is_located() {
    let exact = "a".repeat(64);
    let input = CallbackPanel {
        icon: Some(&exact),
        ..CallbackPanel::new("Label", 1, 1)
    };
    let admitted = callback_panel_input(input).unwrap();
    assert_eq!(
        serde_json::to_value(admitted.view()).unwrap()["nodes"][0]["search_panel"]["sections"][0]["icon"],
        exact
    );
    let over = format!("{exact}x");
    let error = callback_panel_input(CallbackPanel {
        icon: Some(&over),
        ..input
    })
    .unwrap_err();
    let message = "Search panel section 1 exceeds icon byte limit.";
    assert_eq!(
        error,
        erp_views::ViewError::InvalidDescription(message.to_owned())
    );
    assert_eq!(error.to_string(), message);
    assert!(!error.to_string().contains(&over));
}

#[test]
fn custom_callback_view_key_byte_limit_is_located() {
    let exact = format!("{}.{}", "a".repeat(64), "b".repeat(64));
    let input = CallbackPanel {
        view_key: Some(&exact),
        ..CallbackPanel::new("Label", 1, 1)
    };
    let admitted = callback_panel_input(input).unwrap();
    assert_eq!(
        serde_json::to_value(admitted.view()).unwrap()["nodes"][0]["search_panel"]["view_types"][0],
        exact
    );
    let over = format!("{}.{}", "a".repeat(65), "b".repeat(64));
    let error = callback_panel_input(CallbackPanel {
        view_key: Some(&over),
        ..input
    })
    .unwrap_err();
    let message = "Search panel section 0 exceeds view key byte limit.";
    assert_eq!(
        error,
        erp_views::ViewError::InvalidDescription(message.to_owned())
    );
    assert_eq!(error.to_string(), message);
    assert!(!error.to_string().contains(&over));
}

#[test]
fn section_identity_is_positive() {
    let mut source = section_example();
    source["sections"][0]["id"] = serde_json::json!(0);
    let error = serde_json::from_value::<ListSearchPanel>(source).unwrap_err();
    assert_eq!(error.classify(), serde_json::error::Category::Data);
    assert_eq!(
        error.to_string(),
        "invalid value: integer `0`, expected a nonzero u32"
    );
}

#[cfg(feature = "host-access")]
#[test]
fn caller_overlay_checks_values_before_replacing_defaults() {
    use erp_registry::{FieldDecl, ModelDecl, RegistryBuilder};
    use erp_views::wire::{PanelCallerDefaults, PanelValue};
    static FIELDS: &[FieldDecl] = &[
        FieldDecl::selection("category_id", &[("alpha", "Alpha"), ("beta", "Beta")]),
        FieldDecl::text("notes"),
    ];
    static MODEL: ModelDecl = ModelDecl::new("base", "panel.item").fields(FIELDS);
    let registry = RegistryBuilder::new([])
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let model = registry.model("panel.item").unwrap();
    let mut panel: ListSearchPanel = serde_json::from_value(section_example()).unwrap();
    let ListSearchPanel::Sections { sections, .. } = &mut panel else {
        panic!("sections")
    };
    let section = &mut sections[0];
    let defaults =
        PanelCallerDefaults::checked(section, model, vec![PanelValue::Selection("alpha".into())])
            .unwrap();
    section.set_caller_defaults(defaults).unwrap();
    assert_eq!(
        serde_json::to_value(section.defaults()).unwrap(),
        serde_json::json!(["alpha"])
    );
    for (values, message) in [
        (
            vec![PanelValue::Selection("unknown".into())],
            "Search panel section 1 has an invalid selection default.",
        ),
        (
            vec![PanelValue::Record(
                erp_views::wire::WireId::try_from(1).unwrap(),
            )],
            "Search panel section 1 has an invalid selection default.",
        ),
        (
            vec![
                PanelValue::Selection("alpha".into()),
                PanelValue::Selection("beta".into()),
            ],
            "Search panel section 1 exceeds default count limit.",
        ),
    ] {
        let error = PanelCallerDefaults::checked(section, model, values).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(error.message(), message);
        assert_eq!(
            serde_json::to_value(section.defaults()).unwrap(),
            serde_json::json!(["alpha"])
        );
    }
    let defaults = PanelCallerDefaults::checked(section, model, vec![]).unwrap();
    let mut other = section.clone();
    match &mut other {
        erp_views::wire::PanelSection::Category { id, .. } => {
            *id = std::num::NonZeroU32::new(3).unwrap()
        }
        _ => panic!("category"),
    }
    let error = other.set_caller_defaults(defaults).unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Search panel section 3 has caller defaults for another section."
    );
    let mut unsupported = section.clone();
    match &mut unsupported {
        erp_views::wire::PanelSection::Category { field, .. } => field.name = "notes".into(),
        _ => panic!("category"),
    }
    let error = PanelCallerDefaults::checked(&unsupported, model, vec![]).unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Search panel section 1 has an unsupported default field."
    );
}
