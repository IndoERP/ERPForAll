// SPDX-License-Identifier: MIT OR Apache-2.0
// Kanban lowering cases; included into the `h18_lower` test module.

fn kanban_arch(card: &str) -> String {
    format!(r#"<kanban><templates><t t-name="card">{card}</t></templates></kanban>"#)
}

fn kanban_refused(xml: &str) -> String {
    match fixture(xml, "kanban").unwrap_err() {
        ViewError::InvalidDescription(message) => message,
        other => panic!("Unexpected error kind: {other:?}"),
    }
}

#[test]
fn kanban_card_lowers_to_fields_and_regions() {
    let value = fixture(
        &kanban_arch(
            r#"<field name="name"/><main><field name="partner_id"/><field name="count" string="Copies"/></main><aside><field name="name"/></aside><footer><field name="state"/></footer>"#,
        ),
        "kanban",
    )
    .unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    let root = &nodes[0];
    assert_eq!(root["kind"], "kanban");
    for flag in [
        "quick_create",
        "records_draggable",
        "groups_draggable",
        "archivable",
        "can_open",
    ] {
        assert_eq!(root[flag], true, "{flag}");
    }
    assert!(root.get("page_limit").is_none());
    assert!(root.get("default_group_by").is_none());
    assert_eq!(root["children"], serde_json::json!([1]));
    let card = &nodes[1];
    assert_eq!(card["kind"], "kanban_card");
    let order: Vec<(String, String)> = card["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| {
            let node = &nodes[index.as_u64().unwrap() as usize];
            (
                node["kind"].as_str().unwrap().to_owned(),
                node["region"]
                    .as_str()
                    .or(node["field"]["name"].as_str())
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("field", "name"),
            ("kanban_section", "main"),
            ("kanban_section", "aside"),
            ("kanban_section", "footer")
        ]
        .map(|(kind, name)| (kind.to_owned(), name.to_owned()))
    );
    let main = &nodes[card["children"][1].as_u64().unwrap() as usize];
    let fields: Vec<_> = main["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| &nodes[index.as_u64().unwrap() as usize])
        .collect();
    assert_eq!(fields[0]["kind"], "field");
    assert_eq!(fields[0]["widget"], "many2one");
    assert_eq!(fields[1]["label"], "Copies");
    assert_eq!(fields[1]["widget"], "integer");
}

#[test]
fn kanban_root_flags_limit_and_actions() {
    let value = fixture(
        r#"<kanban limit="12" create="0" records_draggable="False" groups_draggable="0" archivable="false" can_open="0"><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#,
        "kanban",
    )
    .unwrap();
    let root = &value["nodes"][0];
    assert_eq!(root["page_limit"], 12);
    assert_eq!(root["create"], false);
    assert_eq!(
        root["quick_create"], false,
        "no quick create without create"
    );
    for flag in [
        "records_draggable",
        "groups_draggable",
        "archivable",
        "can_open",
    ] {
        assert_eq!(root[flag], false, "{flag}");
    }
    let quick = fixture(
        r#"<kanban quick_create="0"><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#,
        "kanban",
    )
    .unwrap();
    assert_eq!(quick["nodes"][0]["quick_create"], false);
    assert!(quick["nodes"][0].get("create").is_none());
}

#[test]
fn kanban_refuses_what_the_card_cannot_carry() {
    let card = kanban_arch;
    for (xml, expected) in [
        (card(r#"<div><field name="name"/></div>"#), "Put custom card markup inside <restricted_templates> under <kanban>."),
        (card(r#"<field name="name" class="fw-bold"/>"#), "'class'"),
        (card(r#"<main t-if="record.name"><field name="name"/></main>"#), "'t-if'"),
        (card(r#"<field name="name" invisible="not name"/>"#), "Use a static value"),
        (card(r#"<field name="child_ids"/>"#), "one2many"),
        (card(r#"<main/><main/>"#), "at most once"),
        (card(r#"<field name="nope"/>"#), "unknown field 'h18.fixture.nope'"),
        (card("text"), "Text content"),
        (
            r#"<kanban><templates><t t-name="menu"><field name="name"/></t></templates></kanban>"#.to_owned(),
            "the field declaration requires t-name=\"card\"",
        ),
        (
            r#"<kanban><field name="name"/><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#.to_owned(),
            "directly under <kanban>",
        ),
        (
            r#"<kanban><t t-name="card"><field name="name"/></t></kanban>"#.to_owned(),
            "inside <templates>",
        ),
        (r#"<kanban><templates/></kanban>"#.to_owned(), "exactly one"),
        (
            r#"<kanban><templates><t t-name="card"><field name="name"/></t><t t-name="card"><field name="name"/></t></templates></kanban>"#.to_owned(),
            "exactly one",
        ),
        (
            r#"<kanban limit="0"><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#.to_owned(),
            "positive whole number",
        ),
    ] {
        let message = kanban_refused(&xml);
        assert!(message.contains(expected), "{xml}: {message}");
    }
    for attribute in [
        "group_create",
        "on_create",
        "quick_create_view",
        "default_order",
        "sample",
        "highlight_color",
    ] {
        let xml = format!(
            r#"<kanban {attribute}="1"><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#
        );
        assert!(
            kanban_refused(&xml).contains(&format!("'{attribute}'")),
            "{attribute}"
        );
    }
}

#[test]
fn kanban_default_group_by_is_one_groupable_field() {
    let xml = |group: &str| {
        format!(
            r#"<kanban default_group_by="{group}"><templates><t t-name="card"><field name="name"/></t></templates></kanban>"#
        )
    };
    let value = fixture(&xml("state"), "kanban").unwrap();
    assert_eq!(value["nodes"][0]["default_group_by"]["name"], "state");
    fixture(&xml("partner_id"), "kanban").unwrap();
    assert!(kanban_refused(&xml("state,stage")).contains("groups by one"));
    assert!(kanban_refused(&xml("ratio")).contains("must be a stored"));
    assert!(kanban_refused(&xml("nope")).contains("unknown field"));
}

#[test]
fn kanban_progressbar_declaration_is_closed() {
    let xml = |bar: &str| {
        format!(
            r#"<kanban>{bar}<templates><t t-name="card"><field name="name"/></t></templates></kanban>"#
        )
    };
    let value = fixture(
        &xml(
            r#"<progressbar field="state" colors='{"done": "success", "draft": "muted"}' sum_field="count" help="Open work"/>"#,
        ),
        "kanban",
    )
    .unwrap();
    let bar = &value["nodes"][1];
    assert_eq!(bar["kind"], "kanban_progressbar");
    assert_eq!(bar["field"]["name"], "state");
    assert_eq!(
        bar["colors"],
        serde_json::json!([{"value": "done", "tone": "success"}, {"value": "draft", "tone": "muted"}])
    );
    assert_eq!(bar["sum_field"]["name"], "count");
    assert_eq!(bar["help"], "Open work");
    fixture(
        &xml(r#"<progressbar field="name" colors="{'a': 'info'}"/>"#),
        "kanban",
    )
    .unwrap();
    for (bar, expected) in [
        (
            r#"<progressbar field="state" colors='{"done": "300"}'/>"#,
            "must be one of success",
        ),
        (
            r#"<progressbar field="state" colors='{"gone": "info"}'/>"#,
            "is not a value of selection",
        ),
        (
            r#"<progressbar field="state" colors='{"done": "info", "done": "danger"}'/>"#,
            "declared twice",
        ),
        (
            r#"<progressbar field="state" colors="[1]"/>"#,
            "literal dictionary",
        ),
        (r#"<progressbar field="state"/>"#, "needs colors"),
        (
            r#"<progressbar field="count" colors="{}"/>"#,
            "selection or char",
        ),
        (
            r#"<progressbar field="state" colors="{}" sum_field="name"/>"#,
            "integer, float or monetary",
        ),
        (
            r#"<progressbar field="state" colors="{}" widget="x"/>"#,
            "'widget'",
        ),
        (
            r#"<progressbar field="state" colors="{}"/><progressbar field="stage" colors="{}"/>"#,
            "at most one",
        ),
    ] {
        let message = kanban_refused(&xml(bar));
        assert!(message.contains(expected), "{bar}: {message}");
    }
}

/// The dashboard card widget is admitted on a text card field and
/// refused by name on any other type.
#[test]
fn kanban_admits_the_dashboard_card_widget_on_text_fields_only() {
    let value = fixture(
        &kanban_arch(r#"<field name="name"/><field name="source" widget="dashboard_card"/>"#),
        "kanban",
    )
    .unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    let card = &nodes[1];
    let field = &nodes[card["children"][1].as_u64().unwrap() as usize];
    assert_eq!(field["kind"], "field");
    assert_eq!(field["field"]["name"], "source");
    assert_eq!(field["widget"], "dashboard_card");
    let message = kanban_refused(&kanban_arch(
        r#"<field name="name" widget="dashboard_card"/>"#,
    ));
    assert!(
        message.contains("Field widget 'dashboard_card' does not support type 'char'."),
        "{message}"
    );
}
