//! Node locator, translated selectors, `hasclass`.

use super::{arch, locate_xml, path_of, select_all};
use crate::error::ViewError;

fn located(base: &str, spec: &str) -> Option<(Vec<usize>, String)> {
    let source = arch(base);
    locate_xml(&source, spec)
        .expect("spec is valid")
        .map(|id| (path_of(&source, id), source.node_xml(id)))
}

#[test]
fn locator_rules_follow_test_node_locator() {
    // test_no_match_xpath / test_match_xpath
    assert_eq!(
        located(
            "<root><foo/><bar/><baz/></root>",
            r#"<xpath expr="//qux"/>"#
        ),
        None
    );
    assert_eq!(
        located(
            "<root><foo/><bar/><baz/></root>",
            r#"<xpath expr="//bar"/>"#
        ),
        Some((vec![1], "<bar/>".to_owned()))
    );
    // test_no_match_field: matched by @name against fields only
    assert_eq!(
        located("<root><foo/><bar/><baz/></root>", r#"<field name="qux"/>"#),
        None
    );
    assert_eq!(
        located(
            r#"<root><field name="foo"/><field name="bar"/><field name="baz"/></root>"#,
            r#"<field name="qux"/>"#
        ),
        None
    );
    // test_match_field
    assert_eq!(
        located(
            r#"<root><field name="foo"/><field name="bar"/><field name="baz"/></root>"#,
            r#"<field name="bar"/>"#
        ),
        Some((vec![1], r#"<field name="bar"/>"#.to_owned()))
    );
    // test_no_match_other / test_match_other
    assert_eq!(located("<root><foo/><bar/><baz/></root>", "<qux/>"), None);
    assert_eq!(
        located("<root><foo/><bar/><baz/></root>", "<bar/>"),
        Some((vec![1], "<bar/>".to_owned()))
    );
    // test_attribute_mismatch
    assert_eq!(
        located(
            r#"<root><foo attr="1"/><bar attr="2"/><baz attr="3"/></root>"#,
            r#"<bar attr="5"/>"#
        ),
        None
    );
    // test_attribute_filter: the second bar, not the first nor the nested one
    assert_eq!(
        located(
            r#"<root><bar attr="1"/><bar attr="2"/><root><bar attr="3"/></root></root>"#,
            r#"<bar attr="2"/>"#
        ),
        Some((vec![1], r#"<bar attr="2"/>"#.to_owned()))
    );
    // test_version_mismatch: a spec attribute missing on the node prevents the match
    assert_eq!(
        located(
            r#"<root version="4"><foo attr="1"/></root>"#,
            r#"<foo attr="1" version="3"/>"#
        ),
        None
    );
}

#[test]
fn locator_picks_the_first_of_several_same_tag_nodes_in_document_order() {
    let base = r#"<form><group><field name="a" widget="x"/></group><field name="a" widget="y"/><field name="b"/></form>"#;
    // field: by name only, other spec attributes ignored, first in document order (depth first)
    assert_eq!(
        located(base, r#"<field name="a" widget="y" position="after"/>"#),
        Some((vec![0, 0], r#"<field name="a" widget="x"/>"#.to_owned()))
    );
    // xpath: first match in document order
    assert_eq!(
        located(base, r#"<xpath expr="//field[@name='a']"/>"#),
        Some((vec![0, 0], r#"<field name="a" widget="x"/>"#.to_owned()))
    );
    // other tags: every spec attribute except `position` must match
    assert_eq!(
        located(base, r#"<group position="inside"/>"#).map(|(p, _)| p),
        Some(vec![0])
    );
    // the root itself can be located
    assert_eq!(located(base, "<form/>").map(|(p, _)| p), Some(vec![]));
}

#[test]
fn locator_compares_mode_like_reference_for_non_field_tags() {
    // only skips `position`: `mode` must match too.
    let base = r#"<form><div name="x"/></form>"#;
    assert_eq!(
        located(base, r#"<div name="x" position="replace" mode="inner"/>"#),
        None
    );
    assert_eq!(
        located(
            r#"<form><field name="x"/></form>"#,
            r#"<field name="x" position="replace" mode="inner"/>"#
        )
        .map(|(p, _)| p),
        Some(vec![0])
    );
}

#[test]
fn field_spec_without_name_is_an_error() {
    let source = arch(r#"<form><field/></form>"#);
    let err = locate_xml(&source, r#"<field position="after"/>"#).unwrap_err();
    assert_eq!(err, ViewError::FieldSpecWithoutName("<field>".to_owned()));
    assert_eq!(
        err.to_string(),
        "Field specification without a 'name' attribute: <field>"
    );
}

#[test]
fn xpath_without_expr_is_an_error() {
    let source = arch("<form/>");
    let err = locate_xml(&source, r#"<xpath position="inside"/>"#).unwrap_err();
    assert_eq!(err, ViewError::MissingXpathExpr);
    assert_eq!(
        err.to_string(),
        "Missing 'expr' attribute in xpath specification"
    );
}

#[test]
fn translated_attributes_cannot_be_selectors() {
    let source = arch(r#"<form><field name="foo" string="Name"/></form>"#);
    let err = locate_xml(
        &source,
        r#"<xpath expr="//field[@string='Name']" position="after"/>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::TranslatedAttributeSelector("string".to_owned())
    );
    assert_eq!(
        err.to_string(),
        "View inheritance may not use attribute 'string' as a selector."
    );
    let err = locate_xml(&source, r#"<field string="Name" position="after"/>"#).unwrap_err();
    assert_eq!(
        err,
        ViewError::TranslatedAttributeSelector("string".to_owned())
    );
    let err = locate_xml(
        &source,
        r#"<xpath expr="//field[@name='foo' and @t-attf-placeholder='x']" position="after"/>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::TranslatedAttributeSelector("t-attf-placeholder".to_owned())
    );
    let err = locate_xml(&source, r#"<page help="Help" position="after"/>"#).unwrap_err();
    assert_eq!(
        err,
        ViewError::TranslatedAttributeSelector("help".to_owned())
    );
}

#[test]
fn translated_selector_rule_scope_matches_reference() {
    let source = arch(r#"<form><field name="foo" string="Name"/><page string="P"/></form>"#);
    // Only nodes with @position are checked (`//*[@position]`).
    assert!(
        locate_xml(&source, r#"<page string="P"/>"#)
            .unwrap()
            .is_some()
    );
    // An empty translated attribute is not a selector (`node.get(attr)` is falsy).
    assert!(
        locate_xml(&source, r#"<field name="foo" string="" position="after"/>"#)
            .unwrap()
            .is_some()
    );
    // A translated name inside a string literal is not an attribute selector.
    assert_eq!(
        locate_xml(
            &source,
            r#"<xpath expr="//field[@name='@string']" position="after"/>"#
        ),
        Ok(None)
    );
}

#[test]
fn hasclass_counts_match_test_xpath_extensions() {
    let source =
        arch(r#"<node class="foo"><node class="foo bar baz"/><node class="foo bar"/></node>"#);
    let count = |expr: &str| select_all(&source, expr).unwrap().len();
    assert_eq!(count(r#"//node[hasclass("foo")]"#), 3);
    assert_eq!(count(r#"//node[hasclass("bar")]"#), 2);
    assert_eq!(count(r#"//node[hasclass("baz")]"#), 1);
    assert_eq!(count(r#"//node[hasclass("foo")][not(hasclass("bar"))]"#), 1);
    assert_eq!(count(r#"//node[hasclass("foo", "baz")]"#), 1);
    // and which nodes
    assert_eq!(
        select_all(&source, r#"//node[hasclass("foo")][not(hasclass("bar"))]"#).unwrap(),
        vec![Vec::<usize>::new()]
    );
    assert_eq!(
        select_all(&source, r#"//node[hasclass("foo", "baz")]"#).unwrap(),
        vec![vec![0]]
    );
}
