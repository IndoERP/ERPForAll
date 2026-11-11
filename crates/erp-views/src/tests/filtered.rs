//! Tests for filtered arch serialization.

use super::arch;
use crate::arch::FilterDecision;

#[test]
fn filtered_xml_allow_all_matches_to_xml() {
    let xml = "<form string=\"Test\"><field name=\"a\"/><field name=\"b\"/></form>";
    let a = arch(xml);
    assert_eq!(a.to_xml_filtered(|_| FilterDecision::Allow), a.to_xml());
}

#[test]
fn filtered_xml_denies_subtree_and_preserves_tail() {
    let xml = "<span>(<field name=\"a\"/>)</span>";
    let a = arch(xml);
    let filtered = a.to_xml_filtered(|node| {
        if a.tag_of(node) == "field" {
            FilterDecision::Deny
        } else {
            FilterDecision::Allow
        }
    });
    assert_eq!(filtered, "<span>()</span>");
}

#[test]
fn filtered_xml_unwraps_t_element() {
    let xml = "<group><t><field name=\"c\"/></t></group>";
    let a = arch(xml);
    let filtered = a.to_xml_filtered(|node| {
        if a.tag_of(node) == "t" {
            FilterDecision::Unwrap
        } else {
            FilterDecision::Allow
        }
    });
    assert_eq!(filtered, "<group><field name=\"c\"/></group>");
}

#[test]
fn filtered_xml_denies_t_element() {
    let xml = "<group><t><field name=\"c\"/></t></group>";
    let a = arch(xml);
    let filtered = a.to_xml_filtered(|node| {
        if a.tag_of(node) == "t" {
            FilterDecision::Deny
        } else {
            FilterDecision::Allow
        }
    });
    assert_eq!(filtered, "<group></group>");
}

#[test]
fn remove_attribute_works() {
    let xml = "<field name=\"a\" groups=\"base.group_user\"/>";
    let mut a = arch(xml);
    assert_eq!(a.attrs_of(a.root_node_idx()).len(), 2);
    a.remove_attribute(a.root_node_idx(), "groups");
    assert_eq!(a.attrs_of(a.root_node_idx()).len(), 1);
    assert_eq!(a.to_xml(), "<field name=\"a\"/>");
}
