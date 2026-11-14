// SPDX-License-Identifier: MIT OR Apache-2.0
#[test]
fn dashboard_declaration_is_typed_and_separate_from_row_fields() {
    let value = fixture(r#"<list><dashboard><count name="available" string="Available" domain="[('state', '=', 'draft')]" color="info"/><kpi name="volume" string="Volume" field="count" aggregate="sum" date_field="deadline"/></dashboard><field name="name"/></list>"#, "list").unwrap();
    assert_eq!(value["nodes"][1]["kind"], "dashboard");
    assert_eq!(value["nodes"][1]["version"], 1);
    assert_eq!(value["nodes"][1]["items"][0]["kind"], "count");
    assert_eq!(value["nodes"][1]["items"][0]["name"], "available");
    assert_eq!(value["nodes"][1]["items"][1]["field"]["name"], "count");
    assert_eq!(value["nodes"][2]["kind"], "field");
    assert_eq!(value["nodes"].as_array().unwrap().len(), 3);
}

#[test]
fn dashboard_refuses_unknown_attributes_duplicates_and_ambiguous_counts() {
    for (xml, message) in [
        (
            r#"<list><dashboard bogus="1"><count name="a" string="A" domain="[]"/></dashboard></list>"#,
            "Unsupported dashboard attribute 'bogus'.",
        ),
        (
            r#"<list><dashboard><count name="a" string="A" domain="[]" filter="a"/></dashboard></list>"#,
            "Dashboard count 'a' requires exactly one domain or named filter.",
        ),
        (
            r#"<list><dashboard><count name="a" string="A" domain="[]"/><count name="a" string="B" domain="[]"/></dashboard></list>"#,
            "Duplicate dashboard item 'a'.",
        ),
        (
            r#"<list><dashboard/></list>"#,
            "Dashboard requires at least one item.",
        ),
        (
            r#"<list><dashboard><kpi name="a" string="A" field="count" aggregate="median" date_field="deadline"/></dashboard></list>"#,
            "Dashboard KPI 'a' has unsupported aggregate 'median'.",
        ),
    ] {
        let error = fixture(xml, "list").unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(error.to_string(), message);
    }
}

fn admit_fixture_dashboard(
    root: erp_sdk::views::NodeRef<'_>,
    spec: &erp_sdk::ModelSpec,
) -> erp_sdk::views::ViewResult<()> {
    for node in root.children().filter(|node| node.tag() == "dashboard") {
        erp_sdk::views::validate_dashboard(node, spec)?;
    }
    Ok(())
}
